// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use crate::{Error, InitializationScript, RGBA};
use crossbeam_channel::*;
use dpi::{PhysicalPosition, PhysicalSize};
use jni::{
  errors::Result as JniResult,
  objects::{GlobalRef, JMap, JObject, JString},
  JNIEnv, JavaVM,
};
use once_cell::sync::Lazy;
use std::{
  collections::BTreeMap,
  ffi::c_void,
  os::unix::prelude::*,
  sync::{Arc, Mutex},
};

use super::{find_class, EvalCallback, WebviewId, EVAL_CALLBACKS, EVAL_ID_GENERATOR, PACKAGE};

pub type ActivityId = i32;

// Unbounded so fire-and-forget senders (notably `InnerWebView::Drop`)
// cannot silently lose teardown messages under burst load. The previous
// `bounded(8)` could swallow `RemoveChildWebView` when many child
// webviews closed rapidly, leaving orphan `PopupWindow`s in the
// WindowManager — webviews stayed floating on screen even after the
// Rust handle dropped. Queue depth and per-message size are both small,
// so unbounded growth is not a memory concern in practice.
static CHANNEL: Lazy<(
  Sender<(ActivityId, WebViewMessage)>,
  Receiver<(ActivityId, WebViewMessage)>,
)> = Lazy::new(unbounded);
pub static MAIN_PIPE: Lazy<[OwnedFd; 2]> = Lazy::new(|| {
  let mut pipe: [RawFd; 2] = Default::default();
  unsafe { libc::pipe(pipe.as_mut_ptr()) };
  unsafe { pipe.map(|fd| OwnedFd::from_raw_fd(fd)) }
});

#[derive(Clone)]
pub struct ActivityProxy {
  pub activity: GlobalRef,
  pub window_manager: GlobalRef,
  pub webview: Option<GlobalRef>,
  pub webchrome_client: GlobalRef,
  pub java_vm: *mut c_void,
}

unsafe impl Send for ActivityProxy {}

impl ActivityProxy {
  pub fn new(
    vm: JavaVM,
    activity: GlobalRef,
    window_manager: GlobalRef,
    webchrome_client: GlobalRef,
  ) -> Self {
    Self {
      activity,
      window_manager,
      webview: None,
      webchrome_client,
      java_vm: vm.get_java_vm_pointer() as *mut _,
    }
  }
}

static ACTIVITY_PROXY: once_cell::sync::Lazy<Mutex<BTreeMap<ActivityId, ActivityProxy>>> =
  Lazy::new(|| Mutex::new(BTreeMap::new()));

/// Per-webview Java view registry — multi-webview child embedding
/// needs to route `SetBounds` / `SetVisible` to the *specific*
/// WebView's PopupWindow. `ActivityProxy.webview` was a single-slot
/// field (upstream assumed one host webview per activity), so when a
/// second `build_as_child` WebView was added the new instance
/// overwrote the previous one. This map keeps every active webview's
/// Java `GlobalRef` keyed by its Rust-side `WebviewId` (the UUID
/// generated in `InnerWebView::new_inner`).
static WEBVIEWS: once_cell::sync::Lazy<Mutex<std::collections::HashMap<WebviewId, GlobalRef>>> =
  Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

pub fn activity_proxy(id: ActivityId) -> Option<ActivityProxy> {
  ACTIVITY_PROXY.lock().unwrap().get(&id).cloned()
}

/// Register a WebView's Java GlobalRef under its WebviewId.
/// Called from `CreateWebView` handler after the Java object is built.
pub fn register_webview(webview_id: WebviewId, webview: GlobalRef) {
  WEBVIEWS.lock().unwrap().insert(webview_id, webview);
}

/// Look up a WebView's Java GlobalRef by WebviewId — used by
/// `SetBounds` / `SetVisible` to address the correct WebView.
pub fn webview_by_id(webview_id: &WebviewId) -> Option<GlobalRef> {
  WEBVIEWS.lock().unwrap().get(webview_id).cloned()
}

/// Drop a WebView from the registry on instance close / activity destroy.
pub fn unregister_webview(webview_id: &WebviewId) {
  WEBVIEWS.lock().unwrap().remove(webview_id);
}

/// Legacy fallback for `WebViewMessage::Jni(None, _)` from top-level
/// `wry::android::dispatch` / `JniHandle::exec` callers that pre-date
/// multi-webview addressing. Returns any registered webview's
/// `GlobalRef` — `HashMap` iteration order is unspecified, so callers
/// must not rely on a specific instance being selected.
fn any_webview() -> Option<GlobalRef> {
  WEBVIEWS.lock().unwrap().values().next().cloned()
}

fn remove_activity_proxy(id: ActivityId) {
  ACTIVITY_PROXY.lock().unwrap().remove(&id);
}

pub fn register_activity_proxy(
  vm: JavaVM,
  id: ActivityId,
  activity: GlobalRef,
  window_manager: GlobalRef,
  webchrome_client: GlobalRef,
) {
  let mut activity_proxy = ACTIVITY_PROXY.lock().unwrap();
  if let Some(proxy) = activity_proxy.get_mut(&id) {
    proxy.activity = activity;
    proxy.window_manager = window_manager;
    proxy.webchrome_client = webchrome_client;
    proxy.java_vm = vm.get_java_vm_pointer() as *mut _;
  } else {
    let proxy = ActivityProxy::new(vm, activity, window_manager, webchrome_client);
    activity_proxy.insert(id, proxy.clone());
  }
}

pub fn activity_id_for_window_manager(window_manager: JObject) -> Option<ActivityId> {
  for (activity_id, proxy) in ACTIVITY_PROXY.lock().unwrap().iter() {
    let vm = unsafe { JavaVM::from_raw(proxy.java_vm.cast()) }.unwrap();
    let mut env = vm.attach_current_thread_as_daemon().unwrap();
    let equals = env
      .call_method(
        proxy.window_manager.as_obj(),
        "equals",
        "(Ljava/lang/Object;)Z",
        &[(&window_manager).into()],
      )
      .and_then(|v| v.z())
      .unwrap_or_default();
    if equals {
      return Some(*activity_id);
    }
  }
  None
}

pub fn first_activity_id() -> Option<ActivityId> {
  ACTIVITY_PROXY.lock().unwrap().keys().next().cloned()
}

pub fn get_webview(activity_id: ActivityId) -> Option<GlobalRef> {
  // Upstream unwraps the proxy lookup, which panics when messages
  // arrive before `CreateWebView` registers the proxy (race) or
  // after `onDestroy` removes it. Propagate `None` instead; callers
  // already match on the `Option`.
  ACTIVITY_PROXY
    .lock()
    .unwrap()
    .get(&activity_id)?
    .webview
    .as_ref()
    .cloned()
}

pub struct MainPipe<'a> {
  pub env: JNIEnv<'a>,
}

impl<'a> MainPipe<'a> {
  pub(crate) fn send(activity_id: ActivityId, message: WebViewMessage) {
    let size = std::mem::size_of::<bool>();
    if let Err(_e) = CHANNEL.0.send((activity_id, message)) {
      // Unbounded channel only fails when the receiver is dropped,
      // i.e. the UI looper is gone. The pipe is also unusable in
      // that state; log instead of pretending the message went out.
      #[cfg(feature = "tracing")]
      tracing::warn!("main_pipe send dropped (looper gone): {_e:?}");
      return;
    }
    unsafe {
      libc::write(
        MAIN_PIPE[1].as_raw_fd(),
        &true as *const _ as *const _,
        size,
      )
    };
  }

  pub fn recv(&mut self) -> JniResult<()> {
    if let Ok((activity_id, message)) = CHANNEL.1.recv() {
      match message {
        WebViewMessage::CreateWebView(attrs) => {
          let Some((activity, web_chrome_client)) =
            activity_proxy(activity_id).map(|p| (p.activity.clone(), p.webchrome_client.clone()))
          else {
            #[cfg(debug_assertions)]
            eprintln!("no activity found for activity id: {}", activity_id);
            return Ok(());
          };
          let CreateWebViewAttributes {
            url,
            html,
            #[cfg(any(debug_assertions, feature = "devtools"))]
            devtools,
            transparent,
            background_color,
            headers,
            on_webview_created,
            autoplay,
            user_agent,
            initialization_scripts,
            id,
            javascript_disabled,
            is_child,
            bounds,
            ..
          } = attrs;

          // Preserve the Rust `WebviewId` before the JNI shadowing on
          // line ~`let id = self.env.new_string(id)?;` turns it into a
          // `JString`. Used at the end of the block to register the
          // GlobalRef in `WEBVIEWS`.
          let attrs_webview_id = id.clone();

          let string_class = self.env.find_class("java/lang/String")?;
          let initialization_scripts_array = self.env.new_object_array(
            initialization_scripts.len() as i32,
            string_class,
            self.env.new_string("")?,
          )?;
          for (i, init_script) in initialization_scripts.into_iter().enumerate() {
            self.env.set_object_array_element(
              &initialization_scripts_array,
              i as i32,
              self.env.new_string(init_script.script)?,
            )?;
          }
          let id = self.env.new_string(id)?;
          // `WryActivity` is now a plain helper wrapping the host
          // `Activity`. `RustWebView` / `RustWebViewClient` constructors
          // expect a `Context` — pass the wrapped host Activity, not
          // the helper instance.
          let host_activity = self
            .env
            .call_method(&activity, "getHost", "()Landroid/app/Activity;", &[])?
            .l()?;
          // Create webview
          let rust_webview_class = find_class(
            &mut self.env,
            &activity,
            format!("{}/RustWebView", PACKAGE.get().unwrap()),
          )?;
          let webview = self.env.new_object(
            &rust_webview_class,
            "(Landroid/content/Context;[Ljava/lang/String;Ljava/lang/String;)V",
            &[
              (&host_activity).into(),
              (&initialization_scripts_array).into(),
              (&id).into(),
            ],
          )?;
          // get settings
          let web_settings = self
            .env
            .call_method(
              &webview,
              "getSettings",
              "()Landroid/webkit/WebSettings;",
              &[],
            )?
            .l()?;
          // set media autoplay
          self.env.call_method(
            &web_settings,
            "setMediaPlaybackRequiresUserGesture",
            "(Z)V",
            &[(!autoplay).into()],
          )?;
          // set user-agent
          if let Some(user_agent) = user_agent {
            let user_agent = self.env.new_string(user_agent)?;
            self.env.call_method(
              &web_settings,
              "setUserAgentString",
              "(Ljava/lang/String;)V",
              &[(&user_agent).into()],
            )?;
          }

          // disable javascript
          if javascript_disabled {
            self.env.call_method(
              &web_settings,
              "setJavaScriptEnabled",
              "(Z)V",
              &[false.into()],
            )?;
          }

          let webview_class_name = format!("{}/RustWebView", PACKAGE.get().unwrap());
          // Skip the back-navigation hook for child webviews — the
          // host activity owns navigation, an overlay webview should
          // not hijack the back button.
          if !is_child {
            self.env.call_method(
              &activity,
              "setWebView",
              format!("(L{webview_class_name};)V"),
              &[(&webview).into()],
            )?;
          }
          // Navigation
          if let Some(u) = url {
            if let Ok(url) = self.env.new_string(u) {
              load_url(&mut self.env, &webview, &url, headers, true)?;
            }
          } else if let Some(h) = html {
            if let Ok(html) = self.env.new_string(h) {
              load_html(&mut self.env, &webview, &html)?;
            }
          }
          // Enable devtools
          #[cfg(any(debug_assertions, feature = "devtools"))]
          self.env.call_static_method(
            &rust_webview_class,
            "setWebContentsDebuggingEnabled",
            "(Z)V",
            &[devtools.into()],
          )?;
          if transparent {
            set_background_color(&mut self.env, &webview, (0, 0, 0, 0))?;
          } else if let Some(color) = background_color {
            set_background_color(&mut self.env, &webview, color)?;
          }
          // Create and set webview client
          let client_class_name = format!("{}/RustWebViewClient", PACKAGE.get().unwrap());
          let rust_webview_client_class =
            find_class(&mut self.env, &activity, client_class_name.clone())?;
          let webview_client = self.env.new_object(
            &rust_webview_client_class,
            format!("(L{webview_class_name};Landroid/content/Context;)V"),
            &[(&webview).into(), (&host_activity).into()],
          )?;
          self.env.call_method(
            &webview,
            "setWebViewClient",
            "(Landroid/webkit/WebViewClient;)V",
            &[(&webview_client).into()],
          )?;
          // set webchrome client
          self.env.call_method(
            &webview,
            "setWebChromeClient",
            "(Landroid/webkit/WebChromeClient;)V",
            &[web_chrome_client.as_obj().into()],
          )?;

          // Add javascript interface (IPC)
          let ipc_class = find_class(
            &mut self.env,
            &activity,
            format!("{}/Ipc", PACKAGE.get().unwrap()),
          )?;
          let ipc = self.env.new_object(
            ipc_class,
            format!("(L{webview_class_name};L{client_class_name};)V"),
            &[(&webview).into(), (&webview_client).into()],
          )?;
          let ipc_str = self.env.new_string("ipc")?;
          self.env.call_method(
            &webview,
            "addJavascriptInterface",
            "(Ljava/lang/Object;Ljava/lang/String;)V",
            &[(&ipc).into(), (&ipc_str).into()],
          )?;

          // Mount: child webviews go into a `PopupWindow` overlay (own
          // ViewRootImpl + Surface, composed by SurfaceFlinger as a
          // peer of the host window — required when the host is a
          // NativeActivity that takes the window Surface for native
          // rendering and bypasses HWUI for decor children). Non-child
          // webviews replace the activity's content view (upstream
          // behavior).
          if is_child {
            let density =
              display_density(&mut self.env, &host_activity).unwrap_or(1.0);
            let (x, y, w, h) = match bounds {
              Some(rect) => {
                let pos: PhysicalPosition<i32> = rect.position.to_physical(density);
                let size: PhysicalSize<i32> = rect.size.to_physical(density);
                (pos.x, pos.y, size.width, size.height)
              }
              None => (0, 0, -1, -1),
            };
            self.env.call_method(
              &activity,
              "addChildWebView",
              "(Landroid/view/View;IIII)V",
              &[
                (&webview).into(),
                x.into(),
                y.into(),
                w.into(),
                h.into(),
              ],
            )?;
          } else {
            self.env.call_method(
              &activity,
              "setContentView",
              "(Landroid/view/View;)V",
              &[(&webview).into()],
            )?;
          }

          if let Some(on_webview_created) = on_webview_created {
            if let Err(_e) = on_webview_created(super::Context {
              env: &mut self.env,
              activity: &activity,
              webview: &webview,
            }) {
              #[cfg(feature = "tracing")]
              tracing::warn!("failed to run webview created hook: {_e}");
            }
          }

          let webview = self.env.new_global_ref(webview)?;

          // Multi-webview: register this WebView's `GlobalRef` under
          // its `WebviewId` so `SetBounds` / `SetVisible` can address
          // the correct instance. `ActivityProxy.webview` (single
          // slot) still holds the *most recently created* webview for
          // backwards compatibility with callers that look up by
          // activity id only. `attrs_webview_id` was captured before
          // the destructure shadow turned `id` into a JNI `JString`.
          register_webview(attrs_webview_id.clone(), webview.clone());

          // `get_mut(...).unwrap()` would panic if `OnDestroy` removed
          // the proxy between the earlier `activity_proxy()` guard
          // and this assignment (race during rotation: activity
          // destroyed + recreated rapidly, both messages on the same
          // pipe). Panicking here kills the UI looper thread and the
          // whole webview system hangs. Drop the assignment gracefully
          // if the proxy is gone; the per-webview `WEBVIEWS` registry
          // has already been updated.
          if let Ok(mut proxies) = ACTIVITY_PROXY.lock() {
            if let Some(proxy) = proxies.get_mut(&activity_id) {
              proxy.webview.replace(webview);
            } else {
              #[cfg(feature = "tracing")]
              tracing::warn!(
                activity_id,
                "CreateWebView: activity proxy gone between guard and write"
              );
            }
          }
        }
        WebViewMessage::Eval(webview_id, script, callback) => {
          if let Some(webview) = webview_by_id(&webview_id) {
            let id = EVAL_ID_GENERATOR.next() as i32;

            #[cfg(feature = "tracing")]
            let span = std::sync::Mutex::new(Some(SendEnteredSpan(
              tracing::debug_span!("wry::eval").entered(),
            )));

            EVAL_CALLBACKS
              .get_or_init(Default::default)
              .lock()
              .unwrap()
              .insert(
                id,
                Box::new(move |result| {
                  #[cfg(feature = "tracing")]
                  span.lock().unwrap().take();

                  if let Some(callback) = &callback {
                    callback(result);
                  }
                }),
              );

            let s = self.env.new_string(script)?;
            self.env.call_method(
              webview.as_obj(),
              "evalScript",
              "(ILjava/lang/String;)V",
              &[id.into(), (&s).into()],
            )?;
          }
        }
        WebViewMessage::SetBackgroundColor(webview_id, background_color) => {
          if let Some(webview) = webview_by_id(&webview_id) {
            set_background_color(&mut self.env, webview.as_obj(), background_color)?;
          }
        }
        WebViewMessage::GetWebViewVersion(tx) => {
          if let Some(activity) = activity_proxy(activity_id).map(|p| p.activity.clone()) {
            match self
              .env
              .call_method(activity, "getVersion", "()Ljava/lang/String;", &[])
              .and_then(|v| v.l())
              .and_then(|s| {
                let s = JString::from(s);
                self
                  .env
                  .get_string(&s)
                  .map(|v| v.to_string_lossy().to_string())
              }) {
              Ok(version) => {
                tx.send(Ok(version)).unwrap();
              }
              Err(e) => tx.send(Err(e.into())).unwrap(),
            }
          } else {
            tx.send(Err(Error::ActivityNotFound)).unwrap();
          }
        }
        WebViewMessage::GetUrl(webview_id, tx) => {
          if let Some(webview) = webview_by_id(&webview_id) {
            let url = self
              .env
              .call_method(webview.as_obj(), "getUrl", "()Ljava/lang/String;", &[])
              .and_then(|v| v.l())
              .and_then(|s| {
                let s = JString::from(s);
                self
                  .env
                  .get_string(&s)
                  .map(|v| v.to_string_lossy().to_string())
              })
              .unwrap_or_default();

            tx.send(url).unwrap()
          }
        }
        WebViewMessage::Jni(webview_id, f) => {
          // Activity is still resolved by `activity_id` (one host activity
          // per process). The webview is resolved per-id from `WEBVIEWS`;
          // `None` falls back to "any registered webview" for legacy
          // single-webview API callers (`wry::android::dispatch`,
          // `JniHandle::exec`) that pre-date multi-webview addressing.
          let activity = activity_proxy(activity_id).map(|p| p.activity.clone());
          let webview = match webview_id {
            Some(id) => webview_by_id(&id),
            None => any_webview(),
          };
          match (activity, webview) {
            (Some(activity), Some(webview)) => {
              f(&mut self.env, &activity, webview.as_obj());
            }
            (Some(activity), None) => {
              f(&mut self.env, &activity, &JObject::null());
            }
            (None, _) => {
              f(&mut self.env, &JObject::null(), &JObject::null());
            }
          }
        }
        WebViewMessage::LoadUrl(webview_id, url, headers) => {
          if let Some(webview) = webview_by_id(&webview_id) {
            let url = self.env.new_string(url)?;
            load_url(&mut self.env, webview.as_obj(), &url, headers, false)?;
          }
        }
        WebViewMessage::ClearAllBrowsingData(webview_id) => {
          if let Some(webview) = webview_by_id(&webview_id) {
            self
              .env
              .call_method(webview, "clearAllBrowsingData", "()V", &[])?;
          }
        }
        WebViewMessage::LoadHtml(webview_id, html) => {
          if let Some(webview) = webview_by_id(&webview_id) {
            let html = self.env.new_string(html)?;
            load_html(&mut self.env, webview.as_obj(), &html)?;
          }
        }
        WebViewMessage::Reload(webview_id) => {
          if let Some(webview) = webview_by_id(&webview_id) {
            reload(&mut self.env, webview.as_obj())?;
          }
        }
        // Bounds + visibility for child (popup-mounted) webviews. The
        // `PopupWindow` owns its own size, so `setChildBounds` on
        // `WryActivity` resizes the popup, not the WebView itself.
        // Routing through the activity (not the WebView) is what lets
        // the host drag-resize an embedded webview without recreating
        // the surface.
        WebViewMessage::SetBounds(webview_id, rect) => {
          if let (Some(proxy), Some(webview)) =
            (activity_proxy(activity_id), webview_by_id(&webview_id))
          {
            let host_activity = self
              .env
              .call_method(
                proxy.activity.as_obj(),
                "getHost",
                "()Landroid/app/Activity;",
                &[],
              )?
              .l()?;
            let density = display_density(&mut self.env, &host_activity).unwrap_or(1.0);
            let pos: PhysicalPosition<i32> = rect.position.to_physical(density);
            let size: PhysicalSize<i32> = rect.size.to_physical(density);
            self.env.call_method(
              proxy.activity.as_obj(),
              "setChildBounds",
              "(Landroid/view/View;IIII)V",
              &[
                (&webview).into(),
                pos.x.into(),
                pos.y.into(),
                size.width.into(),
                size.height.into(),
              ],
            )?;
          }
        }
        WebViewMessage::SetVisible(webview_id, visible) => {
          if let (Some(proxy), Some(webview)) =
            (activity_proxy(activity_id), webview_by_id(&webview_id))
          {
            self.env.call_method(
              proxy.activity.as_obj(),
              "setChildVisible",
              "(Landroid/view/View;Z)V",
              &[(&webview).into(), visible.into()],
            )?;
          }
        }
        // Drop path for a single child WebView. The Kotlin helper
        // `removeChildWebView(view)` dismisses the `PopupWindow` keyed
        // by `view.hashCode()` and clears the per-popup bounds /
        // visibility caches; `unregister_webview` then drops the Java
        // `GlobalRef`, letting GC reclaim the WebView. The activity
        // proxy is *not* removed — sibling webviews still rely on it,
        // and proxy cleanup belongs in `OnDestroy`.
        WebViewMessage::RemoveChildWebView(webview_id) => {
          if let (Some(proxy), Some(webview)) =
            (activity_proxy(activity_id), webview_by_id(&webview_id))
          {
            // call_method failures here are non-fatal — the GlobalRef
            // drop on `unregister_webview` still releases the Java
            // object; log and continue rather than poisoning the loop.
            if let Err(_e) = self.env.call_method(
              proxy.activity.as_obj(),
              "removeChildWebView",
              "(Landroid/view/View;)V",
              &[(&webview).into()],
            ) {
              #[cfg(feature = "tracing")]
              tracing::warn!("removeChildWebView call failed: {_e:?}");
            }
          }
          super::destroy_webview(activity_id, &webview_id);
          unregister_webview(&webview_id);
        }
        WebViewMessage::GetCookies(webview_id, tx, url) => {
          if let Some(webview) = webview_by_id(&webview_id) {
            let url = self.env.new_string(url)?;
            let cookies = self
              .env
              .call_method(
                webview,
                "getCookies",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[(&url).into()],
              )
              .and_then(|v| v.l())
              .and_then(|s| {
                let s = JString::from(s);
                self
                  .env
                  .get_string(&s)
                  .map(|v| v.to_string_lossy().to_string())
              })
              .unwrap_or_default();

            tx.send(
              cookies
                .split("; ")
                .flat_map(|c| cookie::Cookie::parse(c.to_string()))
                .collect(),
            )
            .unwrap();
          }
        }
        WebViewMessage::OnDestroy {
          activity_id,
          webview_id,
          is_changing_configurations,
        } => {
          // keep our webview references (callbacks etc) alive if the activity is going to be recreated due to configuration changes
          // e.g. rotation, multi-window mode change, etc
          if !is_changing_configurations {
            super::destroy_webview(activity_id, &webview_id);
            unregister_webview(&webview_id);
            remove_activity_proxy(activity_id);
          }
        }
      }
    }
    Ok(())
  }
}

fn load_url<'a>(
  env: &mut JNIEnv<'a>,
  webview: &JObject<'a>,
  url: &JString<'a>,
  headers: Option<http::HeaderMap>,
  main_thread: bool,
) -> JniResult<()> {
  let function = if main_thread {
    "loadUrlMainThread"
  } else {
    "loadUrl"
  };
  if let Some(headers) = headers {
    let obj = env.new_object("java/util/HashMap", "()V", &[])?;
    let headers_map = {
      let headers_map = JMap::from_env(env, &obj)?;
      for (name, value) in headers.iter() {
        let key = env.new_string(name)?;
        let value = env.new_string(value.to_str().unwrap_or_default())?;
        headers_map.put(env, &key, &value)?;
      }
      headers_map
    };
    env.call_method(
      webview,
      function,
      "(Ljava/lang/String;Ljava/util/Map;)V",
      &[url.into(), (&headers_map).into()],
    )?;
  } else {
    env.call_method(webview, function, "(Ljava/lang/String;)V", &[url.into()])?;
  }
  Ok(())
}

fn load_html<'a>(env: &mut JNIEnv<'a>, webview: &JObject<'a>, html: &JString<'a>) -> JniResult<()> {
  env.call_method(
    webview,
    "loadHTMLMainThread",
    "(Ljava/lang/String;)V",
    &[html.into()],
  )?;
  Ok(())
}

fn reload<'a>(env: &mut JNIEnv<'a>, webview: &JObject<'a>) -> JniResult<()> {
  env.call_method(webview, "reload", "()V", &[])?;
  Ok(())
}

fn set_background_color<'a>(
  env: &mut JNIEnv<'a>,
  webview: &JObject<'a>,
  (r, g, b, a): RGBA,
) -> JniResult<()> {
  let color = (a as i32) << 24 | (r as i32) << 16 | (g as i32) << 8 | (b as i32);
  env.call_method(webview, "setBackgroundColor", "(I)V", &[color.into()])?;
  Ok(())
}

/// Reads `getResources().getDisplayMetrics().density` (logical →
/// physical px scale factor) off the host activity. Required for
/// child webview bounds: callers send logical coordinates (matching
/// the host's logical coordinate space) and we translate to physical
/// pixels for the `PopupWindow` API.
fn display_density<'a>(env: &mut JNIEnv<'a>, activity: &JObject<'a>) -> JniResult<f64> {
  let resources = env
    .call_method(
      activity,
      "getResources",
      "()Landroid/content/res/Resources;",
      &[],
    )?
    .l()?;
  let metrics = env
    .call_method(
      &resources,
      "getDisplayMetrics",
      "()Landroid/util/DisplayMetrics;",
      &[],
    )?
    .l()?;
  let density = env.get_field(&metrics, "density", "F")?.f()?;
  Ok(density as f64)
}

pub(crate) enum WebViewMessage {
  // `CreateWebView` carries the target `WebviewId` inside the
  // `CreateWebViewAttributes` payload — no extra discriminator field.
  CreateWebView(CreateWebViewAttributes),
  // Each routed message below carries an explicit `WebviewId` selecting
  // its target. Previously these went through `get_webview(activity_id)`
  // which read the single-slot `ActivityProxy.webview` field; under
  // multi-webview that slot always holds the *most recently spawned*
  // webview, so `eval`/`load_url`/`reload`/etc. landed on the wrong
  // instance. With the id on the message the handler uses
  // `webview_by_id` to resolve the right `GlobalRef`.
  Eval(WebviewId, String, Option<EvalCallback>),
  SetBackgroundColor(WebviewId, RGBA),
  GetWebViewVersion(Sender<Result<String, Error>>),
  GetUrl(WebviewId, Sender<String>),
  GetCookies(WebviewId, Sender<Vec<cookie::Cookie<'static>>>, String),
  // `Option<WebviewId>`: per-instance `InnerWebView::exec` passes
  // `Some(id)` to address its own webview; legacy top-level
  // `wry::android::dispatch` and `JniHandle::exec` pass `None` and fall
  // back to "any registered webview" (matches the original single-
  // webview semantics). With `None`, HashMap iteration order picks the
  // target — callers relying on a specific instance must use the id.
  Jni(
    Option<WebviewId>,
    Box<dyn FnOnce(&mut JNIEnv, &JObject, &JObject) + Send>,
  ),
  LoadUrl(WebviewId, String, Option<http::HeaderMap>),
  LoadHtml(WebviewId, String),
  Reload(WebviewId),
  ClearAllBrowsingData(WebviewId),
  /// Update bounds for a child webview (logical px). `WebviewId` selects
  /// which webview's PopupWindow to resize. Routed to
  /// `WryActivity::setChildBounds(View, x, y, w, h)`.
  SetBounds(WebviewId, crate::Rect),
  /// Show / hide a child webview without destroying it. `WebviewId`
  /// selects which webview's PopupWindow to toggle.
  SetVisible(WebviewId, bool),
  /// Tear down a single child webview's `PopupWindow` + UI state.
  /// Sent from `InnerWebView::Drop` so dropping the Rust handle
  /// removes the overlay from the screen and frees the Java-side
  /// `PopupWindow` + `View` references. Without this, dropping a
  /// `WebView` only clears the Rust-side handler maps; the Java
  /// `PopupWindow` keeps the WebView floating in the WindowManager
  /// forever (visible artifact after close, plus a memory leak).
  RemoveChildWebView(WebviewId),
  OnDestroy {
    activity_id: ActivityId,
    webview_id: WebviewId,
    is_changing_configurations: bool,
  },
}

#[derive(Clone)]
pub(crate) struct CreateWebViewAttributes {
  pub id: String,
  pub url: Option<String>,
  pub html: Option<String>,
  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub devtools: bool,
  pub transparent: bool,
  pub background_color: Option<RGBA>,
  pub headers: Option<http::HeaderMap>,
  pub autoplay: bool,
  pub on_webview_created:
    Option<Arc<dyn Fn(super::Context) -> JniResult<()> + Send + Sync + 'static>>,
  pub user_agent: Option<String>,
  pub initialization_scripts: Vec<InitializationScript>,
  pub javascript_disabled: bool,
  /// `true` when constructed via `WebViewBuilder::build_as_child`; the
  /// webview mounts as an overlay in its own `PopupWindow` instead of
  /// replacing the host activity's content view.
  pub is_child: bool,
  /// Initial bounds for child webviews. Logical coordinates against
  /// the host window; `main_pipe` translates to physical px via the
  /// host's display density.
  pub bounds: Option<crate::Rect>,
}

// SAFETY: only use this when you are sure the span will be dropped on the same thread it was entered
#[cfg(feature = "tracing")]
struct SendEnteredSpan(tracing::span::EnteredSpan);

#[cfg(feature = "tracing")]
unsafe impl Send for SendEnteredSpan {}
