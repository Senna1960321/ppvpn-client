//! Android: the app's Kotlin from the UI process (runner/android), over JNI.
//! For now the saved sign-in's key (#473): `com.peakpassvpn.ppvpn.Credentials`
//! seals and opens the blob with an AES-GCM key in the Android Keystore.
//!
//! MainActivity loads this library (System.loadLibrary) before Dart opens
//! it by the same name, which gives Dart that same copy. So JNI_OnLoad
//! runs here, on the VM's thread, where FindClass sees the app's classes:
//! the class is looked up once there. The calls come later from the
//! client's threads, which FindClass would not serve.
#![allow(non_snake_case)] // JNI's names

use std::ffi::c_void;
use std::sync::OnceLock;

use jni::objects::{JByteArray, JClass, JValue};
use jni::refs::{Global, Reference as _};
use jni::strings::JNIStr;
use jni::sys::jint;
use jni::{jni_sig, jni_str, JavaVM};
use ppvpn_client::PlatformError;

static CREDENTIALS: OnceLock<Global<JClass<'static>>> = OnceLock::new();

/// Keeps the VM (jni's singleton) and the app's Credentials class.
///
/// # Safety
/// Called by the VM loading this library, with its own pointer.
#[no_mangle]
pub unsafe extern "system" fn JNI_OnLoad(
    vm: *mut jni::sys::JavaVM,
    _reserved: *mut c_void,
) -> jint {
    let vm = JavaVM::from_raw(vm);
    let found = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let class = env.find_class(jni_str!("com/peakpassvpn/ppvpn/Credentials"))?;
        let _ = CREDENTIALS.set(env.new_global_ref(class)?);
        Ok(())
    });
    if let Err(error) = found {
        tracing::error!(%error, "com.peakpassvpn.ppvpn.Credentials not found: no saved sign-in");
    }
    jni::sys::JNI_VERSION_1_6
}

fn failure(message: impl std::fmt::Display) -> PlatformError {
    PlatformError::Failed {
        message: format!("keystore: {message}"),
    }
}

/// The blob sealed by the app's key (made on first use).
pub(crate) fn seal(blob: &[u8]) -> Result<Vec<u8>, PlatformError> {
    call(jni_str!("seal"), blob)?.ok_or_else(|| failure("seal returned nothing"))
}

/// The blob of a sealed one; None when its key is gone or it does not open
/// with it.
pub(crate) fn open(sealed: &[u8]) -> Result<Option<Vec<u8>>, PlatformError> {
    call(jni_str!("open"), sealed)
}

/// `static byte[] Credentials.<method>(byte[])`, null as None. A Java
/// exception comes back as the error.
fn call(method: &'static JNIStr, input: &[u8]) -> Result<Option<Vec<u8>>, PlatformError> {
    let class = CREDENTIALS
        .get()
        .ok_or_else(|| failure("the app's Credentials class was not found at load"))?;
    let vm = JavaVM::singleton().map_err(failure)?;
    vm.attach_current_thread(|env| -> jni::errors::Result<Option<Vec<u8>>> {
        let input = env.byte_array_from_slice(input)?;
        let output = env
            .call_static_method(class, method, jni_sig!("([B)[B"), &[JValue::Object(&input)])?
            .l()?;
        if output.is_null() {
            return Ok(None);
        }
        let output = env.cast_local::<JByteArray>(output)?;
        Ok(Some(env.convert_byte_array(&output)?))
    })
    .map_err(failure)
}

// The VPN service runs in :tunnel, independently of Flutter's UI process.
// It loads this same library only for PushAgent: no client/sign-in/core is
// created here. Its private lease file is written by the authenticated UI.
mod push {
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    use jni::errors::ThrowRuntimeExAndDefault;
    use jni::objects::{JClass, JObject, JString, JValue};
    use jni::refs::Global;
    use jni::{jni_sig, jni_str, EnvUnowned, JavaVM};
    use ppvpn_client::{
        PushAgent, PushAgentConfig, PushAgentListener, PushAgentState, PushMessage,
    };

    struct Running {
        agent: Arc<PushAgent>,
        thread: JoinHandle<()>,
    }
    static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

    struct Listener(Global<JObject<'static>>);
    impl PushAgentListener for Listener {
        fn on_push(&self, message: PushMessage) -> bool {
            let Ok(vm) = JavaVM::singleton() else {
                return false;
            };
            vm.attach_current_thread(|env| -> jni::errors::Result<bool> {
                let key = JString::from_str(env, message.id.to_string())?;
                let id = JString::from_str(env, message.message_id.map(|id| id.to_string()).unwrap_or_default())?;
                let title = JString::from_str(env, message.title)?;
                let body = JString::from_str(env, message.body)?;
                env.call_method(&self.0, jni_str!("show"),
                    jni_sig!("(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Z"),
                    &[JValue::Object(&key), JValue::Object(&id), JValue::Object(&title), JValue::Object(&body)],
                )?.z()
            }).unwrap_or(false)
        }
        fn on_state(&self, _: PushAgentState) {}
    }

    fn stop() {
        let running = RUNNING.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(running) = running {
            running.agent.stop();
            let _ = running.thread.join();
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_peakpassvpn_ppvpn_NativePushAgent_start<'local>(
        mut env: EnvUnowned<'local>,
        _class: JClass<'local>,
        data_dir: JString<'local>,
        listener: JObject<'local>,
    ) {
        env.with_env(|env| -> jni::errors::Result<()> {
            stop();
            let dir = data_dir.try_to_string(env)?;
            let listener = Arc::new(Listener(env.new_global_ref(&listener)?));
            let agent = PushAgent::new(
                PushAgentConfig {
                    log_dir: std::path::Path::new(&dir)
                        .parent()
                        .unwrap_or(std::path::Path::new(&dir))
                        .join("logs")
                        .to_string_lossy()
                        .into_owned(),
                    data_dir: dir,
                    platform: "android".into(),
                    app_version: env!("CARGO_PKG_VERSION").into(),
                },
                listener,
            );
            let worker = agent.clone();
            let thread = std::thread::spawn(move || worker.run());
            *RUNNING.lock().unwrap_or_else(|p| p.into_inner()) = Some(Running { agent, thread });
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>();
    }

    #[no_mangle]
    pub extern "system" fn Java_com_peakpassvpn_ppvpn_NativePushAgent_stop<'local>(
        mut env: EnvUnowned<'local>,
        _class: JClass<'local>,
    ) {
        env.with_env(|_| -> jni::errors::Result<()> {
            stop();
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>();
    }
}

/// WorkManager's bounded, off-main-thread registration. It shares the UI's
/// singleton client and credential coordinator; no second refresh session
/// or Flutter engine is created, and no VPN service is started here.
#[no_mangle]
pub extern "system" fn Java_com_peakpassvpn_ppvpn_NativePushToken_register<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: JClass<'local>,
    files_dir: jni::objects::JString<'local>,
    token: jni::objects::JString<'local>,
    bundle_id: jni::objects::JString<'local>,
) -> jni::sys::jboolean {
    use jni::errors::ThrowRuntimeExAndDefault;
    env.with_env(|env| -> jni::errors::Result<bool> {
        let dir = files_dir.try_to_string(env)?;
        let token = token.try_to_string(env)?;
        let bundle = bundle_id.try_to_string(env)?;
        let _ = crate::dirs::APP_DIR.set(dir);
        let Ok(bridge) = crate::bridge::start_for_push() else {
            return Ok(false);
        };
        bridge.client.set_push_token(token.clone(), Some(bundle));
        let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < until {
            let snapshot = bridge.client.snapshot();
            match snapshot.auth {
                ppvpn_client::AuthState::SignedOut if snapshot.last_error.is_none() => {
                    return Ok(true)
                }
                ppvpn_client::AuthState::SignedIn
                    if bridge.client.push_token_registered(&token) =>
                {
                    return Ok(true)
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
        Ok(false)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// A fresh onNewToken event, not a queued registration retry: the latter
/// must not override a newer decision that FCM is unavailable.
#[no_mangle]
pub extern "system" fn Java_com_peakpassvpn_ppvpn_NativePushToken_recovered(
    _env: jni::EnvUnowned<'_>,
    _class: JClass<'_>,
) {
    if let Some(bridge) = crate::bridge::running() {
        bridge.client.set_android_push_fallback(false);
    }
}
