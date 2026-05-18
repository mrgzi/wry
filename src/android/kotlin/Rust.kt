// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

@file:Suppress("unused")

package {{package}}

import android.app.Activity
import android.content.Intent
import android.webkit.WebView
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse

// `WryActivity` is no longer an `Activity` subclass — it's a plain
// helper wrapping the host activity. External funs that used to take
// `WryActivity` now take `android.app.Activity`. Lifecycle hooks are
// unused when the host owns its own lifecycle, but keep the JNI
// symbols so callers that still wire them up compile.
object Rust {
    init {
        System.loadLibrary("{{library}}")
    }

    @JvmStatic external fun onActivityCreate(activity: Activity)
    @JvmStatic external fun onActivityDestroy(activity: Activity)
    @JvmStatic external fun onActivitySaveInstanceState()
    @JvmStatic external fun onActivityLowMemory()
    @JvmStatic external fun onWindowFocusChanged(activity: Activity, focus: Boolean)
    @JvmStatic external fun onNewIntent(intent: Intent)

    @JvmStatic external fun create()
    @JvmStatic external fun start()
    @JvmStatic external fun resume()
    @JvmStatic external fun pause()
    @JvmStatic external fun stop()

    @JvmStatic external fun wryCreate()
    @JvmStatic external fun onWebviewDestroy(activity: Activity, webviewId: String)

    @JvmStatic external fun ipc(webviewId: String, url: String, message: String)

    @JvmStatic external fun assetLoaderDomain(webviewId: String): String
    @JvmStatic external fun withAssetLoader(webviewId: String): Boolean
    @JvmStatic external fun handleRequest(webviewId: String, request: WebResourceRequest, isDocumentStartScriptEnabled: Boolean): WebResourceResponse?
    @JvmStatic external fun shouldOverride(webviewId: String, url: String): Boolean
    @JvmStatic external fun onPageLoading(webviewId: String, url: String)
    @JvmStatic external fun onPageLoaded(webviewId: String, url: String)
    @JvmStatic external fun onEval(webviewId: String, id: Int, result: String)

    @JvmStatic external fun handleReceivedTitle(webviewId: String, title: String)
}