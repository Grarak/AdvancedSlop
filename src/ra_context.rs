use crate::cartridge_io::CartridgeIo;
use crate::core::memory::regions;
use crate::utils::{set_thread_prio_affinity, ThreadAffinity, ThreadPriority};
use png::DecodeOptions;
use rcheevos::{
    rc_api_request_t, rc_api_server_response_t, rc_client_begin_identify_and_load_game, rc_client_begin_login_with_password, rc_client_begin_login_with_token, rc_client_create, rc_client_destroy,
    rc_client_do_frame, rc_client_event_t, rc_client_get_game_info, rc_client_get_user_info, rc_client_get_userdata, rc_client_idle, rc_client_server_callback_t, rc_client_set_event_handler,
    rc_client_set_hardcore_enabled, rc_client_set_userdata, rc_client_t, RC_API_SERVER_RESPONSE_RETRYABLE_CLIENT_ERROR, RC_OK,
};
use reqwest::blocking::Client;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{BufReader, Cursor};
use std::os::raw::{c_char, c_int, c_void};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::{fs, mem, ptr, slice, thread};

// RetroAchievements addresses a console through one flat space, not the guest's own
// map, so every read has to be translated. The layout below is rcheevos' own, from
// _rc_memory_regions_gameboy_advance in src/rcheevos/consoleinfo.c — achievement sets
// are authored against exactly these offsets, so it is not ours to choose:
//
//   0x000000-0x007FFF -> 0x03000000   32K  internal work ram
//   0x008000-0x047FFF -> 0x02000000  256K  external work ram
//   0x048000-0x057FFF -> 0x0E000000   64K  game pak save ram
const RA_EWRAM_START: u32 = 0x008000;
const RA_SAVE_START: u32 = 0x048000;
const RA_SAVE_END: u32 = 0x058000;

struct ServerRequest {
    url: String,
    content_type: String,
    data: Option<String>,
    callback: rc_client_server_callback_t,
    callback_data: *mut c_void,
}

#[derive(Clone)]
pub struct LoginCallbackData {
    pub result: i32,
    pub error_message: Option<String>,
}

struct EventRequest {
    title: String,
    description: String,
    badge_name: String,
    badge_url: String,
}

#[derive(Default)]
pub struct RaEvent {
    pub title: String,
    pub description: String,
    pub badge: Option<(Vec<u8>, png::OutputInfo)>,
}

enum Request {
    Server(ServerRequest),
    Event(EventRequest),
}

pub struct RaContext {
    rc_client: *mut rc_client_t,

    // Guest memory for rc_read_mem. The shm is read directly rather than through the
    // fastmem window so reads do not depend on which pages are currently mapped (the
    // smc protection decommits and recommits iwram pages as jit code comes and goes).
    shm_ptr: *const u8,
    cartridge_io: *const CartridgeIo,

    cache_dir: PathBuf,

    requests_sender: Sender<Request>,
    requests_receiver: Receiver<Request>,

    active: Arc<AtomicBool>,
    login_callback_data: Mutex<Option<LoginCallbackData>>,

    pub event: Mutex<(RaEvent, Instant)>,
}

impl RaContext {
    unsafe extern "C" fn rc_read_mem(address: u32, buffer: *mut u8, num_bytes: u32, client: *mut rc_client_t) -> u32 {
        if num_bytes == 0 {
            return 0;
        }
        let context = (rc_client_get_userdata(client) as *mut Self).as_mut().unwrap();
        if context.shm_ptr.is_null() {
            return 0;
        }
        let Some(end) = address.checked_add(num_bytes) else {
            return 0;
        };
        let buf = slice::from_raw_parts_mut(buffer, num_bytes as usize);

        // Reads that straddle two regions are rejected rather than stitched: the regions
        // are adjacent in RA's space but far apart in the guest's, so a straddling read
        // has no single meaning. rcheevos only ever asks for 1/2/4 bytes inside a region.
        let shm_offset = if end <= RA_EWRAM_START {
            regions::IWRAM_REGION.shm_offset + address as usize
        } else if address >= RA_EWRAM_START && end <= RA_SAVE_START {
            regions::EWRAM_REGION.shm_offset + (address - RA_EWRAM_START) as usize
        } else if address >= RA_SAVE_START && end <= RA_SAVE_END {
            // Save ram lives on the cartridge, not in the shm — there is no fastmem
            // mapping to read, so go through the same accessor the bus uses.
            let Some(cartridge_io) = context.cartridge_io.as_ref() else {
                return 0;
            };
            for (i, dst) in buf.iter_mut().enumerate() {
                *dst = cartridge_io.read_save_buf(address - RA_SAVE_START + i as u32);
            }
            return num_bytes;
        } else {
            return 0;
        };

        let src = slice::from_raw_parts(context.shm_ptr.add(shm_offset), num_bytes as usize);
        buf.copy_from_slice(src);
        num_bytes
    }

    unsafe extern "C" fn rc_server_request(request: *const rc_api_request_t, callback: rc_client_server_callback_t, callback_data: *mut c_void, client: *mut rc_client_t) {
        let context = (rc_client_get_userdata(client) as *mut Self).as_mut().unwrap();
        let request = request.as_ref().unwrap();
        let url = CStr::from_ptr(request.url).to_str().unwrap().to_string();
        let content_type = CStr::from_ptr(request.content_type).to_str().unwrap().to_string();
        let data = if request.post_data.is_null() {
            None
        } else {
            Some(CStr::from_ptr(request.post_data).to_str().unwrap().to_string())
        };
        context
            .requests_sender
            .send(Request::Server(ServerRequest {
                url,
                content_type,
                data,
                callback,
                callback_data,
            }))
            .unwrap();
    }

    unsafe extern "C" fn rc_event_handler(event: *const rc_client_event_t, client: *mut rc_client_t) {
        let ra_context = (rc_client_get_userdata(client) as *mut Self).as_mut_unchecked();
        let event = event.as_ref().unwrap();
        match event.type_ {
            rcheevos::RC_CLIENT_EVENT_ACHIEVEMENT_TRIGGERED => {
                let achievement = event.achievement.as_ref().unwrap();
                let title = CStr::from_ptr(achievement.title);
                let description = CStr::from_ptr(achievement.description);
                let badge_name = CStr::from_ptr(achievement.badge_name.as_ptr());
                let badge_url = CStr::from_ptr(achievement.badge_url);
                ra_context.publish_event(EventRequest {
                    title: title.to_str().unwrap().to_string(),
                    description: description.to_str().unwrap().to_string(),
                    badge_name: badge_name.to_str().unwrap().to_string(),
                    badge_url: badge_url.to_str().unwrap().to_string(),
                });
            }
            _ => {}
        }
    }

    unsafe extern "C" fn rc_login_password_callback(result: c_int, error_message: *const c_char, _: *mut rc_client_t, userdata: *mut c_void) {
        let ra_context = (userdata as *mut Self).as_mut_unchecked();
        let mut callback_data = ra_context.login_callback_data.lock().unwrap();
        *callback_data = Some(LoginCallbackData {
            result,
            error_message: if error_message.is_null() {
                None
            } else {
                Some(CStr::from_ptr(error_message).to_str().unwrap_or("").to_string())
            },
        });
    }

    unsafe extern "C" fn rc_login_token_callback(result: c_int, error_message: *const c_char, _: *mut rc_client_t, userdata: *mut c_void) {
        if result != RC_OK {
            let ra_context = (userdata as *mut Self).as_mut_unchecked();
            ra_context.publish_event(EventRequest {
                title: "Failed to login".to_string(),
                description: if error_message.is_null() {
                    "".to_string()
                } else {
                    CStr::from_ptr(error_message).to_str().unwrap_or("").to_string()
                },
                badge_name: "".to_string(),
                badge_url: "".to_string(),
            });
        }
    }

    unsafe extern "C" fn load_game_callback(result: c_int, error_message: *const c_char, client: *mut rc_client_t, userdata: *mut c_void) {
        let ra_context = (userdata as *mut Self).as_mut_unchecked();
        if result != RC_OK {
            ra_context.publish_event(EventRequest {
                title: "Failed to load game".to_string(),
                description: if error_message.is_null() {
                    "".to_string()
                } else {
                    CStr::from_ptr(error_message).to_str().unwrap_or("").to_string()
                },
                badge_name: "".to_string(),
                badge_url: "".to_string(),
            });
        } else {
            let game_info = rc_client_get_game_info(client).as_ref().unwrap();
            let title = CStr::from_ptr(game_info.title);
            let badge_name = CStr::from_ptr(game_info.badge_name);
            let badge_url = CStr::from_ptr(game_info.badge_url);
            ra_context.publish_event(EventRequest {
                title: "Launching".to_string(),
                description: title.to_str().unwrap().to_string(),
                badge_name: badge_name.to_str().unwrap().to_string(),
                badge_url: badge_url.to_str().unwrap().to_string(),
            })
        }
    }

    pub fn new() -> Self {
        unsafe {
            let rc_client = rc_client_create(Some(Self::rc_read_mem), Some(Self::rc_server_request));
            rc_client_set_event_handler(rc_client, Some(Self::rc_event_handler));
            rc_client_set_hardcore_enabled(rc_client, 0);

            let (requests_sender, requests_receiver) = channel();
            RaContext {
                rc_client,

                shm_ptr: ptr::null(),
                cartridge_io: ptr::null(),

                cache_dir: PathBuf::new(),

                requests_sender,
                requests_receiver,

                active: Arc::new(AtomicBool::new(false)),
                login_callback_data: Mutex::new(None),

                event: Mutex::new((RaEvent::default(), Instant::now())),
            }
        }
    }

    pub fn set_cache_dir(&mut self, cache_dir: PathBuf) {
        self.cache_dir = cache_dir;
    }

    pub fn login_with_password(&mut self, name: &str, password: &str) {
        unsafe {
            let ptr = self as *mut _ as _;
            *self.login_callback_data.lock().unwrap() = None;
            rc_client_set_userdata(self.rc_client, ptr);
            rc_client_begin_login_with_password(
                self.rc_client,
                CString::from_str(name).unwrap().as_ptr(),
                CString::from_str(password).unwrap().as_ptr(),
                Some(Self::rc_login_password_callback),
                ptr,
            );
        }
    }

    pub fn login_with_token(&mut self, name: &str, token: &str) {
        unsafe {
            let ptr = self as *mut _ as _;
            *self.login_callback_data.lock().unwrap() = None;
            rc_client_set_userdata(self.rc_client, ptr);
            rc_client_begin_login_with_token(
                self.rc_client,
                CString::from_str(name).unwrap().as_ptr(),
                CString::from_str(token).unwrap().as_ptr(),
                Some(Self::rc_login_token_callback),
                ptr,
            );
        }
    }

    pub fn get_login_callback_data(&self) -> Option<LoginCallbackData> {
        self.login_callback_data.lock().unwrap().clone()
    }

    pub fn get_user_info(&self) -> Option<(String, String)> {
        unsafe {
            let user_info = rc_client_get_user_info(self.rc_client);
            if user_info.is_null() {
                None
            } else {
                let user_info = user_info.as_ref().unwrap();
                let username = CStr::from_ptr(user_info.username).to_str().unwrap().to_string();
                let token = CStr::from_ptr(user_info.token).to_str().unwrap().to_string();
                Some((username, token))
            }
        }
    }

    /// `shm_ptr` is the base of the emulator's physical-memory shm; `cartridge_io` must
    /// outlive the session (it is owned by the Emu, which lives for the whole app).
    pub fn load_game(&mut self, shm_ptr: *const u8, cartridge_io: &CartridgeIo) {
        let cartridge_file_path = CString::from_str(cartridge_io.file_path.to_str().unwrap()).unwrap();
        self.shm_ptr = shm_ptr;
        self.cartridge_io = cartridge_io;
        unsafe {
            rc_client_begin_identify_and_load_game(
                self.rc_client,
                rcheevos::RC_CONSOLE_GAMEBOY_ADVANCE,
                cartridge_file_path.as_ptr(),
                ptr::null(),
                cartridge_io.rom_len as _,
                Some(Self::load_game_callback),
                self as *mut _ as _,
            );
        }
    }

    /// Detach the guest memory pointers when a game ends, so a stale cartridge cannot be
    /// read while the next one is loading.
    pub fn unload_game(&mut self) {
        self.shm_ptr = ptr::null();
        self.cartridge_io = ptr::null();
    }

    pub fn on_frame(&self) {
        unsafe { rc_client_do_frame(self.rc_client) };
    }

    pub fn on_idle(&self) {
        unsafe { rc_client_idle(self.rc_client) };
    }

    fn publish_event(&self, event: EventRequest) {
        if event.badge_url.is_empty() {
            *self.event.lock().unwrap() = (
                RaEvent {
                    title: event.title,
                    description: event.description,
                    badge: None,
                },
                Instant::now(),
            );
        } else {
            self.requests_sender.send(Request::Event(event)).unwrap();
        }
    }

    pub fn start_server_request_receive_thread(&mut self) -> RaThreadGuard {
        self.active.store(true, Ordering::Release);

        let ptr = self as *mut _ as usize;
        thread::Builder::new()
            .name("retroachievements_requests".to_string())
            .spawn(move || {
                set_thread_prio_affinity(ThreadPriority::Low, &[ThreadAffinity::Core0, ThreadAffinity::Core1]);

                const USER_AGENT: &str = concat!("AdvancedSlop/", env!("CARGO_PKG_VERSION"));

                let ra_context = unsafe { (ptr as *mut RaContext).as_mut_unchecked() };
                let http_client = Client::new();

                while ra_context.active.load(Ordering::Acquire) {
                    let Ok(rc_request) = ra_context.requests_receiver.recv_timeout(Duration::from_millis(500)) else {
                        continue;
                    };

                    match rc_request {
                        Request::Server(rc_request) => {
                            let request = match rc_request.data {
                                None => http_client.get(rc_request.url),
                                Some(data) => http_client.post(rc_request.url).body(data),
                            }
                            .header("User-Agent", USER_AGENT)
                            .header("Content-Type", rc_request.content_type)
                            .timeout(Duration::from_secs(120))
                            .build()
                            .unwrap();

                            let response = http_client.execute(request);
                            match response {
                                Ok(response) => {
                                    if let Some(callback) = rc_request.callback {
                                        let status = response.status().as_u16();
                                        match response.bytes() {
                                            Ok(bytes) => {
                                                let mut rc_response: rc_api_server_response_t = unsafe { mem::zeroed() };
                                                rc_response.body = bytes.as_ptr() as _;
                                                rc_response.body_length = bytes.len();
                                                rc_response.http_status_code = status as _;
                                                unsafe { callback(&rc_response, rc_request.callback_data) };
                                            }
                                            Err(err) => {
                                                let mut rc_response: rc_api_server_response_t = unsafe { mem::zeroed() };
                                                let msg = err.to_string() + "\nPlease check your internet connection";
                                                rc_response.body = msg.as_ptr() as _;
                                                rc_response.body_length = msg.len();
                                                rc_response.http_status_code = RC_API_SERVER_RESPONSE_RETRYABLE_CLIENT_ERROR as _;
                                                unsafe { callback(&rc_response, rc_request.callback_data) };
                                            }
                                        }
                                    }
                                }
                                Err(err) => {
                                    if let Some(callback) = rc_request.callback {
                                        let mut rc_response: rc_api_server_response_t = unsafe { mem::zeroed() };
                                        let msg = err.to_string() + "\nPlease check your internet connection";
                                        rc_response.body = msg.as_ptr() as _;
                                        rc_response.body_length = msg.len();
                                        rc_response.http_status_code = RC_API_SERVER_RESPONSE_RETRYABLE_CLIENT_ERROR as _;
                                        unsafe { callback(&rc_response, rc_request.callback_data) };
                                    }
                                }
                            }
                        }
                        Request::Event(rc_request) => {
                            debug_assert!(!rc_request.badge_name.is_empty() && !rc_request.badge_url.is_empty());
                            let game_info = unsafe { rc_client_get_game_info(ra_context.rc_client).as_ref().unwrap() };
                            let hash = unsafe { CStr::from_ptr(game_info.hash).to_str().unwrap() };

                            let cache_path = ra_context.cache_dir.join(hash).join(&rc_request.badge_name);
                            let mut decode_options = DecodeOptions::default();
                            decode_options.set_ignore_checksums(true);
                            let badge = if cache_path.exists() {
                                let decoder = png::Decoder::new_with_options(BufReader::new(File::open(cache_path).unwrap()), decode_options);
                                let mut reader = decoder.read_info().unwrap();
                                let mut buf = vec![0; reader.output_buffer_size().unwrap()];
                                let info = reader.next_frame(&mut buf).unwrap();
                                buf.truncate(info.buffer_size());
                                buf.shrink_to_fit();
                                Some((buf, info))
                            } else {
                                let request = http_client.get(rc_request.badge_url).timeout(Duration::from_secs(20)).build().unwrap();
                                match http_client.execute(request) {
                                    Ok(response) => {
                                        fs::create_dir_all(ra_context.cache_dir.join(hash)).unwrap();

                                        match response.bytes() {
                                            Ok(bytes) => {
                                                let file = File::create_new(cache_path).unwrap();
                                                file.write_all_at(&bytes, 0).unwrap();

                                                let decoder = png::Decoder::new_with_options(Cursor::new(bytes), decode_options);
                                                let mut reader = decoder.read_info().unwrap();
                                                let mut buf = vec![0; reader.output_buffer_size().unwrap()];
                                                let info = reader.next_frame(&mut buf).unwrap();
                                                buf.truncate(info.buffer_size());
                                                buf.shrink_to_fit();
                                                Some((buf, info))
                                            }
                                            Err(_) => None,
                                        }
                                    }
                                    Err(_) => None,
                                }
                            };

                            *ra_context.event.lock().unwrap() = (
                                RaEvent {
                                    title: rc_request.title,
                                    description: rc_request.description,
                                    badge,
                                },
                                Instant::now(),
                            );
                        }
                    }
                }
            })
            .map(|handle| RaThreadGuard {
                active: Arc::clone(&self.active),
                handle: Some(handle),
            })
            .unwrap()
    }
}

/// Join-on-drop guard for the request thread. The thread reads the RaContext through a raw
/// pointer into actual_main's stack frame, so it MUST be joined before RaContext drops —
/// including when a panic unwinds actual_main (letting it run past the unwind is a
/// use-after-free that corrupts the channel and aborts, burying the real panic). Bind the
/// guard AFTER the RaContext so drop order joins the thread first.
pub struct RaThreadGuard {
    active: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for RaThreadGuard {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            // The thread only observes `active` between requests (≤500ms recv timeout); a
            // panicked thread must not double-panic the unwind.
            let _ = handle.join();
        }
    }
}

impl Drop for RaContext {
    fn drop(&mut self) {
        unsafe { rc_client_destroy(self.rc_client) };
    }
}
