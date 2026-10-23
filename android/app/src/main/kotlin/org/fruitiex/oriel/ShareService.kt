package org.fruitiex.oriel

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.util.Log
import android.view.Surface
import android.view.WindowManager

/**
 * Sharing the phone's screen: holds the MediaProjection (a foreground
 * service, as Android requires) and encodes the screen with a hardware
 * encoder whose input surface the projection renders into. Encoded frames
 * go to the Rust host (OrielActivity.nativeEncodedFrame).
 */
class ShareService : Service() {
    private var projection: MediaProjection? = null
    private var thread: HandlerThread? = null
    private var handler: Handler? = null
    private var display: VirtualDisplay? = null
    private var codec: MediaCodec? = null
    private var input: Surface? = null
    private var drain: Thread? = null
    @Volatile private var draining = false
    private var config: ByteArray? = null
    private var dpi = 0

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == STOP) {
            stopSelf()
            return START_NOT_STICKY
        }
        if (projection != null || intent == null) return START_NOT_STICKY
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, "Screen sharing", NotificationManager.IMPORTANCE_LOW),
        )
        val stop = PendingIntent.getService(
            this, 0, Intent(this, ShareService::class.java).setAction(STOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = Notification.Builder(this, CHANNEL)
            .setContentTitle("oriel is sharing this phone")
            .setContentText("Paired devices can see and control the screen")
            .setSmallIcon(applicationInfo.icon)
            .addAction(Notification.Action.Builder(null, "Stop", stop).build())
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(1, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
        } else {
            startForeground(1, notification)
        }
        val t = HandlerThread("oriel-share").also { it.start() }
        thread = t
        handler = Handler(t.looper)
        val mpm = getSystemService(MediaProjectionManager::class.java)
        @Suppress("DEPRECATION")
        val data: Intent? = intent.getParcelableExtra("data")
        val p = data?.let { mpm.getMediaProjection(intent.getIntExtra("code", 0), it) }
        if (p == null) {
            stopSelf()
            return START_NOT_STICKY
        }
        projection = p
        p.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                stopSelf()
            }
        }, handler)
        val bounds = getSystemService(WindowManager::class.java).maximumWindowMetrics.bounds
        screenWidth = bounds.width()
        screenHeight = bounds.height()
        // Encoders want sizes in whole macroblocks.
        shareWidth = screenWidth and 15.inv()
        shareHeight = screenHeight and 15.inv()
        dpi = resources.displayMetrics.densityDpi
        instance = this
        OrielActivity.nativeShareStarted(shareWidth, shareHeight)
        return START_NOT_STICKY
    }

    @Synchronized
    fun startEncoder(mime: String, width: Int, height: Int, fps: Int, kbps: Int): Boolean {
        stopEncoder()
        return try {
            val format = MediaFormat.createVideoFormat(mime, width, height).apply {
                setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
                setInteger(MediaFormat.KEY_BIT_RATE, kbps * 1000)
                setInteger(MediaFormat.KEY_FRAME_RATE, minOf(fps, 60))
                // Keyframes when the host asks; otherwise rarely.
                setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 30)
                // A still screen produces no frames; repeat the last one now
                // and then so the picture sharpens.
                setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 200_000)
                setInteger(MediaFormat.KEY_PRIORITY, 0)
                setInteger(MediaFormat.KEY_LATENCY, 1)
            }
            val c = MediaCodec.createEncoderByType(mime)
            c.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            val surface = c.createInputSurface()
            c.start()
            codec = c
            input = surface
            // A projection makes one virtual display (Android 14+); later
            // encoders take it over.
            val d = display
            if (d == null) {
                display = projection!!.createVirtualDisplay(
                    "oriel", width, height, dpi,
                    DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, surface, null, handler,
                )
            } else {
                d.resize(width, height, dpi)
                d.surface = surface
            }
            config = null
            draining = true
            drain = Thread({ drain(c) }, "oriel-encode").also { it.start() }
            true
        } catch (e: Exception) {
            Log.e("oriel", "cannot start the encoder", e)
            stopEncoder()
            false
        }
    }

    private fun drain(c: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        while (draining) {
            val index = try {
                c.dequeueOutputBuffer(info, 10_000)
            } catch (_: IllegalStateException) {
                return
            }
            if (index < 0) continue
            val buffer = c.getOutputBuffer(index)
            var bytes = ByteArray(info.size)
            buffer?.position(info.offset)
            buffer?.get(bytes)
            c.releaseOutputBuffer(index, false)
            if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                config = bytes
                continue
            }
            val key = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
            val csd = config
            if (key && csd != null) bytes = csd + bytes
            OrielActivity.nativeEncodedFrame(bytes, key, 0)
        }
    }

    @Synchronized
    fun stopEncoder() {
        draining = false
        drain?.join(500)
        drain = null
        display?.surface = null
        codec?.let {
            try {
                it.stop()
            } catch (_: Exception) {
                // Already stopped.
            }
            it.release()
        }
        codec = null
        input?.release()
        input = null
    }

    @Synchronized
    fun requestKeyframe() {
        codec?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
    }

    @Synchronized
    fun setBitrate(kbps: Int) {
        codec?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, kbps * 1000) })
    }

    override fun onDestroy() {
        stopEncoder()
        display?.release()
        display = null
        projection?.stop()
        projection = null
        thread?.quitSafely()
        instance = null
        OrielActivity.nativeShareStopped()
        super.onDestroy()
    }

    companion object {
        @Volatile var instance: ShareService? = null
        /** The real screen, and the size shared (whole macroblocks). */
        @Volatile var screenWidth = 0
        @Volatile var screenHeight = 0
        @Volatile var shareWidth = 0
        @Volatile var shareHeight = 0

        /** From the shared (encoded) size to the screen's. */
        val scaleX get() = if (shareWidth > 0) screenWidth / shareWidth.toFloat() else 1f
        val scaleY get() = if (shareHeight > 0) screenHeight / shareHeight.toFloat() else 1f

        private const val CHANNEL = "share"
        private const val STOP = "org.fruitiex.oriel.STOP_SHARING"
    }
}
