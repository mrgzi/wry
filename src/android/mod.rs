// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use super::{PageLoadEvent, WebViewAttributes, RGBA};
use crate::{
  custom_protocol_workaround, inject_initialization_scripts::inject_scripts_into_html, Error,
  RequestAsyncResponder, Result,
};
use crossbeam_channel::*;

use http::{Request, Response as HttpResponse};
use jni::{
  errors::Result as JniResult,
  objects::{GlobalRef, JClass, JObject},
  JNIEnv,
};
use ndk::looper::ThreadLooper;
use once_cell::sync::{Lazy, OnceCell};
use raw_window_handle::HasWindowHandle;
use std::{
  borrow::Cow,
  collections::HashMap,
  sync::{mpsc::channel, Mutex},
  time::Duration,
};

pub(crate) mod binding;
mod main_pipe;
use main_pipe::{
  activity_id_for_window_manager, first_activity_id, register_activity_proxy, ActivityId,
  CreateWebViewAttributes, MainPipe, WebViewMessage,
};

use crate::util::Counter;

static COUNTER: Counter = Counter::new();
const MAIN_PIPE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Context<'a, 'b> {
  pub env: &'a mut JNIEnv<'b>,
  pub activity: &'a JObject<'b>,
  pub webview: &'a JObject<'b>,
}

type WebviewId = String;

macro_rules! define_static_handlers {
  ($($key: ident, $var:ident = $type_name:ident);+ $(;)?) => {
    $(static $var: Lazy<Mutex<HashMap<$key, $type_name>>> = Lazy::new(||Mutex::new(HashMap::new()));)*
  };

  ($($var:ident = $type_name:ident { $($fields:ident:$types:ty),+ $(,)? });+ $(;)?) => {
    $(
    static $var: Lazy<Mutex<HashMap<WebviewId, $type_name>>> = Lazy::new(||Mutex::new(HashMap::new()));
    pub struct $type_name {
      $($fields: $types,)*
    }
    impl $type_name {
      pub fn new($($fields: $types,)*) -> Self {
        Self {
          $($fields,)*
        }
      }
    }
    unsafe impl Send for $type_name {}
    unsafe impl Sync for $type_name {})*
  };
}

define_static_handlers! {
  IPC = UnsafeIpc { handler: Box<dyn Fn(Request<String>)> };
  REQUEST_HANDLER = UnsafeRequestHandler { handler:  Box<dyn Fn(&str, Request<Vec<u8>>, bool) -> Option<HttpResponse<Cow<'static, [u8]>>>> };
  TITLE_CHANGE_HANDLER = UnsafeTitleHandler { handler: Box<dyn Fn(String)> };
  URL_LOADING_OVERRIDE = UnsafeUrlLoadingOverride { handler: Box<dyn Fn(String) -> bool> };
  ON_LOAD_HANDLER = UnsafeOnPageLoadHandler { handler: Box<dyn Fn(PageLoadEvent, String)> };
}
define_static_handlers! {
  WebviewId, WITH_ASSET_LOADER = bool;
  WebviewId, ASSET_LOADER_DOMAIN = String;
  // Keyed by `WebviewId`, NOT `ActivityId`. Keying by activity id
  // collapses every child webview on the same host activity to one
  // shared slot: webview B's `insert(activity_id, attrs)` overwrites
  // webview A's entry, and webview A's `destroy_webview` then wipes
  // webview B's attributes too. Per-webview keying keeps each
  // instance's attributes isolated. Reconstruction lookups (e.g.
  // configuration change) iterate the map explicitly.
  WebviewId, WEBVIEW_ATTRIBUTES = CreateWebViewAttributes;
}

pub(crate) static PACKAGE: OnceCell<String> = OnceCell::new();

type EvalCallback = Box<dyn Fn(String) + Send + 'static>;

pub static EVAL_ID_GENERATOR: Counter = Counter::new();
pub static EVAL_CALLBACKS: OnceCell<Mutex<HashMap<i32, EvalCallback>>> = OnceCell::new();

pub fn destroy_webview(_activity_id: ActivityId, webview_id: &WebviewId) {
  // `WEBVIEW_ATTRIBUTES` is now keyed by `WebviewId` so each
  // instance's attributes survive a sibling teardown. `_activity_id`
  // is kept in the signature for ABI stability; future callers that
  // need a per-activity sweep should iterate the map explicitly.
  WEBVIEW_ATTRIBUTES.lock().unwrap().remove(webview_id);
  IPC.lock().unwrap().remove(webview_id);
  REQUEST_HANDLER.lock().unwrap().remove(webview_id);
  TITLE_CHANGE_HANDLER.lock().unwrap().remove(webview_id);
  URL_LOADING_OVERRIDE.lock().unwrap().remove(webview_id);
  ON_LOAD_HANDLER.lock().unwrap().remove(webview_id);
  WITH_ASSET_LOADER.lock().unwrap().remove(webview_id);
  ASSET_LOADER_DOMAIN.lock().unwrap().remove(webview_id);
}

/// Sets up the necessary logic for wry to be able to create the webviews later.
///
/// This function must be run on the thread where the [`JNIEnv`] is registered and the looper is local,
/// hence the requirement for a [`ThreadLooper`].
pub unsafe fn android_setup(
  package: &str,
  mut env: JNIEnv,
  _looper: &ThreadLooper,
  activity: GlobalRef,
) {
  PACKAGE.get_or_init(move || package.to_string());

  let vm = env.get_java_vm().unwrap();

  // `WryActivity` no longer extends `Activity` (helper class wrapping
  // the host). Use the overridden `hashCode()` for the proxy key.
  let activity_id = env
    .call_method(activity.as_obj(), "hashCode", "()I", &[])
    .unwrap()
    .i()
    .unwrap();

  let window_manager = env
    .call_method(
      &activity,
      "getWindowManager",
      "()Landroid/view/WindowManager;",
      &[],
    )
    .unwrap()
    .l()
    .unwrap();
  let window_manager = env.new_global_ref(window_manager).unwrap();

  // `RustWebChromeClient` constructor now takes `android.app.Activity`,
  // not `WryActivity`. Reach through `getHost()` and hand over the
  // wrapped host activity.
  let host_activity = env
    .call_method(&activity, "getHost", "()Landroid/app/Activity;", &[])
    .unwrap()
    .l()
    .unwrap();
  let rust_webchrome_client_class = find_class(
    &mut env,
    activity.as_obj(),
    format!("{}/RustWebChromeClient", PACKAGE.get().unwrap()),
  )
  .unwrap();
  let webchrome_client = env
    .new_object(
      &rust_webchrome_client_class,
      "(Landroid/app/Activity;)V",
      &[(&host_activity).into()],
    )
    .unwrap();

  let webchrome_client = env.new_global_ref(webchrome_client).unwrap();

  register_activity_proxy(vm, activity_id, activity, window_manager, webchrome_client);

  // Mark the looper parameter as intentionally unused at this point —
  // upstream relies on Java-side `WryLifecycleObserver` calling
  // `Rust.wryCreate` from the UI thread to install the FD callback.
  // Hosts without that lifecycle bridge call
  // `wire_main_pipe_on_ui_thread` themselves.
  let _ = _looper;

  // Re-issue `CreateWebView` for every cached attribute set. With the
  // previous `ActivityId`-keyed map only one entry could survive — now
  // each webview has its own slot, so hosts with multiple embedded
  // webviews (rotation, process re-attach) re-create *all* of them
  // instead of resurrecting only the last one. Cloning into a `Vec`
  // first releases the map lock before sending into the main pipe.
  let attributes_snapshot: Vec<CreateWebViewAttributes> = WEBVIEW_ATTRIBUTES
    .lock()
    .unwrap()
    .values()
    .cloned()
    .collect();
  for attrs in attributes_snapshot {
    MainPipe::send(activity_id, WebViewMessage::CreateWebView(attrs));
  }
}

/// Register the MainPipe FD callback on a foreign (UI) thread looper.
///
/// Upstream wry expects `Rust.wryCreate` to be invoked from the UI
/// thread (typically by the `WryLifecycleObserver.onCreate` hook
/// installed by the abstract `WryActivity.onCreate`). Hosts that own
/// their own activity lifecycle — the helper-class `WryActivity`
/// constructed manually in `android_setup` — must wire the pipe
/// themselves. WebView Java constructors require
/// `Looper.myLooper() != null`; without a UI-thread looper handler,
/// `RustWebView`'s `new Handler()` NPEs because the host's render
/// thread has no Java looper.
///
/// `vm` must be a live `JavaVM`; `ui_looper` an `ndk::looper::ForeignLooper`
/// for the Android main thread (`AndroidApp::java_main_looper()`).
///
/// # Errors
/// Returns whatever `Looper::add_fd` errors with.
pub fn wire_main_pipe_on_ui_thread(
  vm: jni::JavaVM,
  ui_looper: &ndk::looper::ForeignLooper,
) -> std::result::Result<(), ndk::looper::LooperError> {
  use std::os::fd::{AsFd, AsRawFd};

  let fd = main_pipe::MAIN_PIPE[0].as_fd();
  ui_looper.add_fd_with_callback(
    fd,
    ndk::looper::FdEvent::INPUT,
    move |fd, _event| {
      let size = std::mem::size_of::<bool>();
      let mut wake = false;
      let n = unsafe { libc::read(fd.as_raw_fd(), &mut wake as *mut _ as *mut _, size) };
      if n != size as libc::ssize_t {
        // Read mismatch — keep the handler registered; this is
        // recoverable (partial wakeup byte) and unregistering would
        // strand pending MainPipe messages.
        return true;
      }
      // Attach this thread to the JVM so MainPipe.recv() can use JNI.
      let env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(_) => return true,
      };
      let mut main_pipe = main_pipe::MainPipe { env };
      main_pipe.recv().is_ok()
    },
  )
}

pub(crate) struct InnerWebView {
  id: String,
  pub activity_id: ActivityId,
}

impl InnerWebView {
  pub fn new_as_child(
    window: &impl HasWindowHandle,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
  ) -> Result<Self> {
    Self::new_inner(window, attributes, pl_attrs, true)
  }

  pub fn new(
    window: &impl HasWindowHandle,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
  ) -> Result<Self> {
    Self::new_inner(window, attributes, pl_attrs, false)
  }

  fn new_inner(
    window: &impl HasWindowHandle,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
    is_child: bool,
  ) -> Result<Self> {
    // Reach for the activity proxy via `first_activity_id()` — the
    // upstream `activity_id_for_window_manager` path reinterprets the
    // raw `ANativeWindow*` from the `RawWindowHandle::AndroidNdk` arm
    // as a Java `WindowManager` and lets JNI call `.equals()` on it.
    // CheckJNI rejects that on every recent NDK build (the pointer is
    // a `ANativeWindow`, not a Java object). Verify the handle is
    // AndroidNdk so calling code stays explicit about the platform,
    // then fall back to the only registered activity. We don't support
    // multiple host activities on Android.
    match window.window_handle()?.as_raw() {
      raw_window_handle::RawWindowHandle::AndroidNdk(_) => {}
      _ => return Err(Error::UnsupportedWindowHandle),
    };
    let activity_id = first_activity_id().expect("no available activity");
    let bounds = attributes.bounds;
    let WebViewAttributes {
      url,
      html,
      initialization_scripts,
      ipc_handler,
      #[cfg(any(debug_assertions, feature = "devtools"))]
      devtools,
      custom_protocols,
      background_color,
      transparent,
      headers,
      autoplay,
      user_agent,
      javascript_disabled,
      ..
    } = attributes;

    let super::PlatformSpecificWebViewAttributes {
      on_webview_created,
      with_asset_loader,
      asset_loader_domain,
      https_scheme,
    } = pl_attrs;

    let http_or_https = if https_scheme { "https" } else { "http" };

    let url = if let Some(mut url) = url {
      if let Some((protocol, _)) = url.split_once("://") {
        if custom_protocols.contains_key(protocol) {
          url = custom_protocol_workaround::apply_uri_work_around(&url, http_or_https, protocol)
        }
      }

      Some(url)
    } else {
      None
    };

    let id = attributes
      .id
      .map(|id| id.to_string())
      .unwrap_or_else(|| COUNTER.next().to_string());

    WITH_ASSET_LOADER
      .lock()
      .unwrap()
      .insert(id.clone(), with_asset_loader);
    if let Some(domain) = asset_loader_domain {
      ASSET_LOADER_DOMAIN
        .lock()
        .unwrap()
        .insert(id.clone(), domain);
    }

    let initialization_scripts_ = initialization_scripts.clone();
    REQUEST_HANDLER
      .lock()
      .unwrap()
      .insert(
        id.clone(),
        UnsafeRequestHandler::new(Box::new(
          move |webview_id: &str, mut request, is_document_start_script_enabled| {
            let uri = request.uri().to_string();
            if let Some((custom_protocol, custom_protocol_handler)) =
              custom_protocols.iter().find(|(protocol, _)| {
                custom_protocol_workaround::is_work_around_uri(&uri, http_or_https, protocol)
              })
            {
              let uri_res = custom_protocol_workaround::revert_uri_work_around(
                &uri,
                http_or_https,
                custom_protocol,
              )
              .parse();

                if let Ok(uri) = uri_res {
                  *request.uri_mut() = uri;
                }

              let (tx, rx) = channel();
              let initialization_scripts = initialization_scripts_.clone();
              let responder: Box<dyn FnOnce(HttpResponse<Cow<'static, [u8]>>)> =
                Box::new(move |mut response| {
                  if !is_document_start_script_enabled {
                    #[cfg(feature = "tracing")]
                    tracing::info!("`addDocumentStartJavaScript` is not supported; injecting initialization scripts via custom protocol handler");
                    response = inject_scripts_into_html(response, &initialization_scripts);
                  }
                  let _ = tx.send(response);
                });

              (custom_protocol_handler)(webview_id, request, RequestAsyncResponder { responder });
              // 3x the timeout while we monitor https://github.com/tauri-apps/wry/issues/1551
              // TODO: Remove timeout
              return rx.recv_timeout(MAIN_PIPE_TIMEOUT * 3).inspect_err(|e| {eprintln!("custom protocol timed out: {e}");}).ok();
            }
            None
          },
      )));

    if let Some(i) = ipc_handler {
      IPC
        .lock()
        .unwrap()
        .insert(id.clone(), UnsafeIpc::new(Box::new(i)));
    }

    if let Some(i) = attributes.document_title_changed_handler {
      TITLE_CHANGE_HANDLER
        .lock()
        .unwrap()
        .insert(id.clone(), UnsafeTitleHandler::new(i));
    }

    if let Some(i) = attributes.navigation_handler {
      URL_LOADING_OVERRIDE
        .lock()
        .unwrap()
        .insert(id.clone(), UnsafeUrlLoadingOverride::new(i));
    }

    if let Some(h) = attributes.on_page_load_handler {
      ON_LOAD_HANDLER
        .lock()
        .unwrap()
        .insert(id.clone(), UnsafeOnPageLoadHandler::new(h));
    }

    let attributes = CreateWebViewAttributes {
      id: id.clone(),
      url,
      html,
      #[cfg(any(debug_assertions, feature = "devtools"))]
      devtools,
      background_color,
      transparent,
      headers,
      on_webview_created,
      autoplay,
      user_agent,
      initialization_scripts,
      javascript_disabled,
      is_child,
      bounds,
    };

    // Key the attribute cache by the per-webview id so hosts with
    // multiple embedded webviews keep one slot per instance. It used
    // to be keyed by `activity_id`, which collapsed every webview on
    // the same host activity to a single shared slot — siblings
    // overwrote each other, and one webview's destroy wiped them all.
    // `attributes.id` is the same id this `InnerWebView` returns from
    // `id()` and is unique per `new_inner` call.
    WEBVIEW_ATTRIBUTES
      .lock()
      .unwrap()
      .insert(attributes.id.clone(), attributes.clone());

    MainPipe::send(activity_id, WebViewMessage::CreateWebView(attributes));

    Ok(Self { id, activity_id })
  }

  pub fn print(&self) -> crate::Result<()> {
    Ok(())
  }

  pub fn id(&self) -> crate::WebViewId<'_> {
    &self.id
  }

  pub fn url(&self) -> crate::Result<String> {
    let (tx, rx) = bounded(1);
    MainPipe::send(
      self.activity_id,
      WebViewMessage::GetUrl(self.id.clone(), tx),
    );
    rx.recv_timeout(MAIN_PIPE_TIMEOUT).map_err(Into::into)
  }

  pub fn eval(&self, js: &str, callback: Option<impl Fn(String) + Send + 'static>) -> Result<()> {
    MainPipe::send(
      self.activity_id,
      WebViewMessage::Eval(
        self.id.clone(),
        js.into(),
        callback.map(|c| Box::new(c) as Box<dyn Fn(String) + Send + 'static>),
      ),
    );
    Ok(())
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn open_devtools(&self) {}

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn close_devtools(&self) {}

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn is_devtools_open(&self) -> bool {
    false
  }

  pub fn zoom(&self, _scale_factor: f64) -> Result<()> {
    Ok(())
  }

  pub fn set_background_color(&self, background_color: RGBA) -> Result<()> {
    MainPipe::send(
      self.activity_id,
      WebViewMessage::SetBackgroundColor(self.id.clone(), background_color),
    );
    Ok(())
  }

  pub fn load_url(&self, url: &str) -> Result<()> {
    MainPipe::send(
      self.activity_id,
      WebViewMessage::LoadUrl(self.id.clone(), url.to_string(), None),
    );
    Ok(())
  }

  pub fn load_url_with_headers(&self, url: &str, headers: http::HeaderMap) -> Result<()> {
    MainPipe::send(
      self.activity_id,
      WebViewMessage::LoadUrl(self.id.clone(), url.to_string(), Some(headers)),
    );
    Ok(())
  }

  pub fn load_html(&self, html: &str) -> Result<()> {
    MainPipe::send(
      self.activity_id,
      WebViewMessage::LoadHtml(self.id.clone(), html.to_string()),
    );
    Ok(())
  }

  pub fn reload(&self) -> Result<()> {
    MainPipe::send(self.activity_id, WebViewMessage::Reload(self.id.clone()));
    Ok(())
  }

  pub fn clear_all_browsing_data(&self) -> Result<()> {
    MainPipe::send(
      self.activity_id,
      WebViewMessage::ClearAllBrowsingData(self.id.clone()),
    );
    Ok(())
  }

  pub fn cookies_for_url(&self, url: &str) -> Result<Vec<cookie::Cookie<'static>>> {
    let (tx, rx) = bounded(1);
    MainPipe::send(
      self.activity_id,
      WebViewMessage::GetCookies(self.id.clone(), tx, url.to_string()),
    );
    rx.recv_timeout(MAIN_PIPE_TIMEOUT).map_err(Into::into)
  }

  pub fn set_cookie(&self, #[allow(unused)] cookie: &cookie::Cookie<'_>) -> Result<()> {
    // Unsupported
    Ok(())
  }

  pub fn delete_cookie(&self, #[allow(unused)] cookie: &cookie::Cookie<'_>) -> Result<()> {
    // Unsupported
    Ok(())
  }

  pub fn cookies(&self) -> Result<Vec<cookie::Cookie<'static>>> {
    Ok(Vec::new())
  }

  pub fn bounds(&self) -> Result<crate::Rect> {
    // PopupWindow owns its layout — caller doesn't read live bounds
    // back from Android. Return the default to keep the API total.
    Ok(crate::Rect::default())
  }

  pub fn set_bounds(&self, bounds: crate::Rect) -> Result<()> {
    MainPipe::send(
      self.activity_id,
      WebViewMessage::SetBounds(self.id.clone(), bounds),
    );
    Ok(())
  }

  pub fn set_visible(&self, visible: bool) -> Result<()> {
    MainPipe::send(
      self.activity_id,
      WebViewMessage::SetVisible(self.id.clone(), visible),
    );
    Ok(())
  }

  pub fn focus(&self) -> Result<()> {
    // Unsupported
    Ok(())
  }

  pub fn focus_parent(&self) -> Result<()> {
    // Unsupported
    Ok(())
  }
}

impl Drop for InnerWebView {
  fn drop(&mut self) {
    // Child (`build_as_child`) webviews each own a Kotlin-side
    // `PopupWindow`. Without an explicit teardown the WindowManager
    // keeps the popup attached even though the Rust handle is gone:
    // the host drops the `WebView` but the webview stays floating on
    // screen. Route a typed message through `MainPipe` so the JNI
    // call lands on the UI thread (Kotlin `runOnUiThread` would not
    // be re-entrant from Rust here), and let the handler call
    // `WryActivity::removeChildWebView(view)` + `unregister_webview`
    // to release the Java refs.
    //
    // Non-child mode (full-screen webview replacing the activity's
    // content view) is also routed through this path. The Kotlin
    // helper is a no-op for views that were never popup-mounted
    // (lookup by `view.hashCode()` misses), so it is safe to send
    // unconditionally — and the registry cleanup still runs.
    MainPipe::send(
      self.activity_id,
      WebViewMessage::RemoveChildWebView(self.id.clone()),
    );
  }
}

#[derive(Clone, Copy)]
pub struct JniHandle {
  pub(crate) activity_id: ActivityId,
}

impl JniHandle {
  /// Execute jni code on the thread of the webview.
  /// Provided function will be provided with the jni evironment, Android activity and WebView.
  ///
  /// Legacy single-webview entry — picks any registered webview.
  /// Callers that own multiple embedded webviews should use
  /// `InnerWebView::exec` which addresses a specific instance.
  pub fn exec<F>(&self, func: F)
  where
    F: FnOnce(&mut JNIEnv, &JObject, &JObject) + Send + 'static,
  {
    MainPipe::send(
      self.activity_id,
      WebViewMessage::Jni(None, Box::new(func)),
    );
  }
}

pub fn platform_webview_version() -> Result<String> {
  let (tx, rx) = bounded(1);
  let activity_id = loop {
    match first_activity_id() {
      Some(id) => break id,
      None => {
        std::thread::sleep(Duration::from_millis(100));
      }
    }
  };
  MainPipe::send(activity_id, WebViewMessage::GetWebViewVersion(tx));
  rx.recv_timeout(MAIN_PIPE_TIMEOUT)?
}

/// Finds a class in the project scope by going through the host
/// activity's `ClassLoader`. The original `WryActivity.getAppClass()`
/// helper is no longer required — when the host is a plain
/// `android.app.Activity` (e.g. `NativeActivity`), it has no such
/// method, and the daemon-thread `JNIEnv` is bound to the boot
/// `ClassLoader`, which can't see app dex classes. Route through the
/// activity's loader instead.
pub fn find_class<'a>(
  env: &mut JNIEnv<'a>,
  activity: &JObject<'_>,
  name: String,
) -> JniResult<JClass<'a>> {
  let class_loader = env
    .call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
    .l()?;
  let class_name = env.new_string(name.replace('/', "."))?;
  let loaded = env
    .call_method(
      &class_loader,
      "loadClass",
      "(Ljava/lang/String;)Ljava/lang/Class;",
      &[(&class_name).into()],
    )?
    .l()?;
  Ok(loaded.into())
}

/// Dispatch a closure to run on the Android context.
///
/// The closure takes the JNI env, the Android activity instance and the possibly null webview.
/// Legacy single-webview entry — callers that own multiple embedded
/// webviews should reach for `InnerWebView::exec` so the closure
/// receives the correct instance's view.
pub fn dispatch<F>(func: F)
where
  F: FnOnce(&mut JNIEnv, &JObject, &JObject) + Send + 'static,
{
  MainPipe::send(
    first_activity_id().expect("no available activity"),
    WebViewMessage::Jni(None, Box::new(func)),
  );
}
