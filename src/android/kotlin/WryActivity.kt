// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

package {{package}}

import android.annotation.SuppressLint
import android.app.Activity
import android.content.Intent
import android.graphics.drawable.ColorDrawable
import android.os.Build
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.webkit.WebView
import android.widget.FrameLayout
import android.widget.PopupWindow

// Upstream wry exposes `WryActivity` as `abstract class WryActivity :
// AppCompatActivity()`, which forces the host application to subclass
// it — a hard incompatibility with hosts that already pick their own
// `Activity` base (notably `android.app.NativeActivity` via the
// `android-activity` crate, used by game engines and any
// winit-on-Android setup). We turn `WryActivity` into a plain helper
// that wraps the host's `Activity` instance. Lifecycle is owned by
// the host; `wry::android_setup` constructs a `WryActivity` manually
// and Rust keeps a `GlobalRef` to it. All JNI call sites against this
// class go through the methods below.
class WryActivity(val host: Activity) {
    private var mWebView: RustWebView? = null
    // PopupWindow holding each child WebView when mounted as overlay.
    // PopupWindow internally calls `WindowManager.addView` with its own
    // `Window` / `ViewRootImpl` / `Surface`, which SurfaceFlinger
    // composes as a peer of the host activity's window — sidestepping
    // the HWUI bypass NativeActivity imposes on its own Surface.
    //
    // Keyed by `View.hashCode()` (the RustWebView Java object's identity
    // hash). Each `build_as_child` WebView builds a separate RustWebView,
    // so the map holds one popup per instance, enabling concurrent
    // overlays.
    private val mPopups: HashMap<Int, PopupWindow> = HashMap()
    // Per-popup last-applied bounds + visibility cache. Hosts driving
    // the render loop call `setChildBounds` / `setChildVisible` every
    // frame (~60 Hz); `PopupWindow.update` + `view.visibility` on no-op
    // churn the ViewRootImpl / Surface lifecycle and we observed
    // webviews vanishing mid-drag. Skip when nothing changed.
    private val mLastBounds: HashMap<Int, IntArray> = HashMap()
    private val mLastVisible: HashMap<Int, Boolean> = HashMap()
    val id: Int = host.hashCode()
    val handleBackNavigation: Boolean = false

    // `wry::android_setup` keys the ACTIVITY_PROXY map by `hashCode()`.
    // Two `WryActivity` instances wrapping the same host must share the
    // same key, otherwise proxy lookups miss.
    override fun hashCode(): Int = host.hashCode()
    override fun equals(other: Any?): Boolean {
        if (other === this) return true
        if (other is WryActivity) return host === other.host
        return false
    }

    fun onWebViewCreate(webView: WebView) { }

    fun setWebView(webView: RustWebView) {
        mWebView = webView
        onWebViewCreate(webView)
    }

    val version: String
        @SuppressLint("WebViewApiAvailability", "ObsoleteSdkInt")
        get() {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                return WebView.getCurrentWebViewPackage()?.versionName ?: ""
            }
            var webViewPackage = "com.google.android.webview"
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
                webViewPackage = "com.android.chrome"
            }
            try {
                @Suppress("DEPRECATION")
                val info = host.packageManager.getPackageInfo(webViewPackage, 0)
                return info.versionName.toString()
            } catch (ex: Exception) {
                Logger.warn("Unable to get package info for '$webViewPackage'$ex")
            }
            try {
                @Suppress("DEPRECATION")
                val info = host.packageManager.getPackageInfo("com.android.webview", 0)
                return info.versionName.toString()
            } catch (ex: Exception) {
                Logger.warn("Unable to get package info for 'com.android.webview'$ex")
            }
            return ""
        }

    // Rust uses `find_class` which now goes through `getClassLoader()
    // .loadClass(name)`; this helper is kept for parity but is not on
    // the hot path.
    fun getAppClass(name: String): Class<*> {
        return Class.forName(name, true, host.classLoader)
    }

    fun getWindowManager(): android.view.WindowManager = host.windowManager
    fun getPackageManager(): android.content.pm.PackageManager = host.packageManager
    fun getClassLoader(): ClassLoader = host.classLoader
    fun isChangingConfigurations(): Boolean = host.isChangingConfigurations
    fun runOnUiThread(action: Runnable) = host.runOnUiThread(action)

    // Default mount path — replaces the host's content view with the
    // WebView. Hosts that retain ownership of the content view (any
    // setup where the host already drives its own GL/Vulkan surface)
    // should use `WebViewBuilder::build_as_child` so
    // `main_pipe::CreateWebView` routes through `addChildWebView`
    // instead.
    fun setContentView(view: View) {
        host.runOnUiThread { host.setContentView(view) }
    }

    // Child-mount path. Wraps the WebView in a fresh `FrameLayout` and
    // shows it via a `PopupWindow` anchored at the host decor view's
    // top-left. `x`/`y` are physical pixels relative to that anchor;
    // `w`/`h` of `-1` collapse to `MATCH_PARENT`.
    //
    // The `FrameLayout` indirection matters: PopupWindow detaches and
    // re-attaches its content view across `showAtLocation`/`update`,
    // and handing the WebView directly to PopupWindow breaks under
    // those re-parent cycles. The wrapper isolates the WebView from
    // PopupWindow's parent churn.
    //
    // Each `build_as_child` WebView gets its own `PopupWindow` in
    // `mPopups`, keyed by `view.hashCode()`. Concurrent overlays are
    // supported — `addChildWebView` for a *new* view does not dismiss
    // prior popups. Calling for the *same* view (re-entrant build of
    // the same RustWebView, rare) dismisses + replaces.
    fun addChildWebView(view: View, x: Int, y: Int, w: Int, h: Int) {
        host.runOnUiThread {
            val key = view.hashCode()
            mPopups.remove(key)?.dismiss()

            val width = if (w <= 0) ViewGroup.LayoutParams.MATCH_PARENT else w
            val height = if (h <= 0) ViewGroup.LayoutParams.MATCH_PARENT else h

            view.visibility = View.VISIBLE
            view.isFocusable = true
            view.isFocusableInTouchMode = true

            // Detach WebView from any prior parent — `showAtLocation`
            // refuses content views that already have a parent.
            (view.parent as? ViewGroup)?.removeView(view)
            val container = FrameLayout(host)
            container.addView(
                view,
                FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.MATCH_PARENT
                )
            )

            // `focusable=false` constructor — `true` makes the popup grab
            // input focus exclusively; tapping outside its bounds drops
            // focus and the OS dismisses the popup as a side effect
            // (tapping anywhere outside the webview vanishes it). With
            // `false` the popup shares focus with the host window; the
            // WebView still receives touch events because it requests
            // focus on touch internally.
            val popup = PopupWindow(container, width, height, false)
            // ColorDrawable required for pre-API-29 `update()` resize
            // to apply (a null background drawable on those releases
            // skips popup re-measurement).
            popup.setBackgroundDrawable(ColorDrawable(0))
            popup.isOutsideTouchable = false
            popup.isClippingEnabled = false
            popup.isTouchable = true
            // Disable default fade/scale so swaps don't blink.
            popup.animationStyle = 0
            popup.showAtLocation(host.window.decorView, Gravity.TOP or Gravity.START, x, y)
            mPopups[key] = popup

            view.requestFocus()
        }
    }

    // Resize a child `PopupWindow`. View identity (`hashCode()`)
    // selects which popup; no-op if not registered. Skip the
    // JNI → `PopupWindow.update` path when bounds did not change
    // since the last frame.
    fun setChildBounds(view: View, x: Int, y: Int, w: Int, h: Int) {
        host.runOnUiThread {
            val key = view.hashCode()
            val popup = mPopups[key] ?: return@runOnUiThread
            val width = if (w <= 0) ViewGroup.LayoutParams.MATCH_PARENT else w
            val height = if (h <= 0) ViewGroup.LayoutParams.MATCH_PARENT else h
            val cached = mLastBounds[key]
            if (cached != null && cached[0] == x && cached[1] == y &&
                cached[2] == width && cached[3] == height) {
                return@runOnUiThread
            }
            mLastBounds[key] = intArrayOf(x, y, width, height)
            popup.update(x, y, width, height)
        }
    }

    // Show / hide a specific child WebView popup without tearing it down.
    //
    // `popup.dismiss()` permanently invalidates the `PopupWindow` —
    // its internal token, `ViewRootImpl`, and Surface are released by
    // the WindowManager. A subsequent `showAtLocation` rebuilds them
    // but the wrapped WebView's GL surface state (GPU textures, JS
    // engine VM state) can race with the recreate cycle; we observed
    // entire webview content vanishing after a single hide → show
    // cycle. Use `contentView.visibility = GONE` instead to keep the
    // `PopupWindow` alive across visibility toggles.
    fun setChildVisible(view: View, visible: Boolean) {
        host.runOnUiThread {
            val key = view.hashCode()
            val popup = mPopups[key] ?: return@runOnUiThread
            if (mLastVisible[key] == visible) return@runOnUiThread
            mLastVisible[key] = visible
            val content = popup.contentView ?: return@runOnUiThread
            content.visibility = if (visible) View.VISIBLE else View.GONE
        }
    }

    // Detach + dismiss a specific child WebView popup. Caller is the
    // Rust drop path for `InnerWebView`.
    fun removeChildWebView(view: View) {
        host.runOnUiThread {
            val key = view.hashCode()
            mPopups.remove(key)?.dismiss()
            mLastBounds.remove(key)
            mLastVisible.remove(key)
        }
    }

    fun startActivity(cls: Class<*>): Int {
        val intent = Intent(host, cls)
        val id = kotlin.random.Random.nextInt()
        host.startActivity(intent)
        return id
    }

    {{class-extension}}
}
