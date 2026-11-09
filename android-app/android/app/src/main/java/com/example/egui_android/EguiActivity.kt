package com.example.egui_android

import android.app.Activity
import android.content.ContentValues
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.provider.MediaStore
import android.provider.OpenableColumns
import androidx.activity.OnBackPressedCallback
import com.google.androidgamesdk.GameActivity
import java.io.File

/**
 * Activity для GL-режима (GameActivity).
 *
 * 除了框架自带的“返回键交给 Rust 决定”和 saved-state JNI 桥之外，
 * 这里还额外提供了三件框架没有的能力：
 *
 *  - [pickImages]      ：拉起系统文件选择器（可多选 JPEG），把选中的图片复制到
 *                        `cacheDir/picked/`，让 Rust 侧拿到真实文件路径；
 *  - [takePickedPaths] ：Rust 每次轮询取走新增路径（取走即清空）；
 *  - [savePathsToGallery]：把转换结果写进系统相册 `Pictures/JPG2UltraHDR`。
 *
 * 之所以要复制文件，是因为 framework 没有任何 SAF/Uri 支持，
 * 而 Rust 端的转换函数只接受文件系统路径。
 */
class EguiActivity : GameActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        instance = this

        // Перехватываем системный Back — не даём GameActivity
        // завершить Activity. Rust-код сам решит, когда выйти.
        onBackPressedDispatcher.addCallback(
            this,
            object : OnBackPressedCallback(true) {
                override fun handleOnBackPressed() {
                    // Back 事件本身会作为输入事件传给 Rust，
                    // 这里只要不调用 finish() 就好。
                }
            }
        )

        // Восстанавливаем состояние после kill/restore
        val savedBytes = savedInstanceState?.getByteArray(SAVED_STATE_KEY)
        nativeSetSavedState(savedBytes)
        logSavedState("onCreate", savedBytes)
    }

    override fun onDestroy() {
        if (instance === this) instance = null
        super.onDestroy()
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        val bytes = nativeGetSavedState()
        if (bytes != null) {
            outState.putByteArray(SAVED_STATE_KEY, bytes)
        }
        logSavedState("onSaveInstanceState", bytes)
    }

    // ─── 文件选择 ────────────────────────────────────────────────

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQ_PICK) return
        if (resultCode != Activity.RESULT_OK || data == null) return

        val uris = ArrayList<Uri>()
        data.clipData?.let { clip ->
            for (i in 0 until clip.itemCount) uris.add(clip.getItemAt(i).uri)
        }
        data.data?.let { uris.add(it) }
        if (uris.isEmpty()) return

        val outDir = File(cacheDir, "picked").apply { mkdirs() }
        val paths = ArrayList<String>()
        for (uri in uris) {
            try {
                val name = queryDisplayName(uri) ?: "picked_${System.currentTimeMillis()}.jpg"
                val safe = name.replace('/', '_').replace('\\', '_')
                val target = uniqueFile(outDir, safe)
                contentResolver.openInputStream(uri)?.use { input ->
                    target.outputStream().use { output -> input.copyTo(output) }
                }
                paths.add(target.absolutePath)
            } catch (e: Exception) {
                android.util.Log.e(TAG, "读取选择的图片失败: $uri", e)
            }
        }
        synchronized(lock) {
            pickedResult = if (pickedResult.isEmpty()) {
                paths.joinToString("\n")
            } else if (paths.isEmpty()) {
                pickedResult
            } else {
                pickedResult + "\n" + paths.joinToString("\n")
            }
        }
    }

    private fun queryDisplayName(uri: Uri): String? {
        return try {
            contentResolver.query(uri, null, null, null, null)?.use { cursor ->
                val idx = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (idx >= 0 && cursor.moveToFirst()) cursor.getString(idx) else null
            }
        } catch (e: Exception) {
            null
        }
    }

    private fun uniqueFile(dir: File, name: String): File {
        var f = File(dir, name)
        if (!f.exists()) return f
        val dot = name.lastIndexOf('.')
        val stem = if (dot > 0) name.substring(0, dot) else name
        val ext = if (dot > 0) name.substring(dot) else ""
        var i = 1
        while (f.exists()) {
            f = File(dir, "${stem}_$i$ext")
            i++
        }
        return f
    }

    // ─── 相册保存 ────────────────────────────────────────────────

    private fun saveOne(path: String): Boolean {
        val file = File(path)
        if (!file.exists()) return false
        val values = ContentValues().apply {
            put(MediaStore.Images.Media.DISPLAY_NAME, file.name)
            put(MediaStore.Images.Media.MIME_TYPE, "image/jpeg")
            put(MediaStore.Images.Media.RELATIVE_PATH, "Pictures/JPG2UltraHDR")
            put(MediaStore.Images.Media.IS_PENDING, 1)
        }
        val resolver = contentResolver
        val uri = resolver.insert(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, values)
            ?: return false
        return try {
            resolver.openOutputStream(uri)?.use { out ->
                file.inputStream().use { it.copyTo(out) }
            }
            values.clear()
            values.put(MediaStore.Images.Media.IS_PENDING, 0)
            resolver.update(uri, values, null, null)
            true
        } catch (e: Exception) {
            android.util.Log.e(TAG, "写入相册失败: $path", e)
            resolver.delete(uri, null, null)
            false
        }
    }

    private fun logSavedState(method: String, bytes: ByteArray?) {
        if (bytes != null) {
            android.util.Log.i(TAG, "$method: saved_state = ${bytes.size} 字节")
        } else {
            android.util.Log.i(TAG, "$method: saved_state = null")
        }
    }

    // ─── JNI 方法（由 Rust 侧实现）─────────────────────────────────

    private external fun nativeGetSavedState(): ByteArray?
    private external fun nativeSetSavedState(bytes: ByteArray?)

    companion object {
        private const val TAG = "EguiActivity"
        private const val SAVED_STATE_KEY = "egui_saved_state"
        private const val REQ_PICK = 0x4A32

        private val lock = Any()
        private var pickedResult: String = ""

        @Volatile
        private var instance: EguiActivity? = null

        /** Rust 调用：拉起系统图片选择器。 */
        @JvmStatic
        fun pickImages() {
            val act = instance
            if (act == null) {
                android.util.Log.w(TAG, "pickImages: activity 还没准备好")
                return
            }
            act.runOnUiThread {
                val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
                    addCategory(Intent.CATEGORY_OPENABLE)
                    type = "image/*"
                    putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
                }
                try {
                    act.startActivityForResult(intent, REQ_PICK)
                } catch (e: Exception) {
                    android.util.Log.e(TAG, "无法拉起文件选择器", e)
                }
            }
        }

        /** Rust 调用：取走新增的图片路径（换行分隔，取走即清空）。 */
        @JvmStatic
        fun takePickedPaths(): String = synchronized(lock) {
            val r = pickedResult
            pickedResult = ""
            r
        }

        /** Rust 调用：把若干文件写入系统相册，返回成功数量。 */
        @JvmStatic
        fun savePathsToGallery(joinedPaths: String): Int {
            val act = instance ?: return 0
            var ok = 0
            for (p in joinedPaths.split('\n')) {
                val path = p.trim()
                if (path.isEmpty()) continue
                if (act.saveOne(path)) ok++
            }
            return ok
        }
    }
}
