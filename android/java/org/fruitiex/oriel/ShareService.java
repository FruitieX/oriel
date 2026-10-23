package org.fruitiex.oriel;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.graphics.Rect;
import android.hardware.display.DisplayManager;
import android.hardware.display.VirtualDisplay;
import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaFormat;
import android.media.projection.MediaProjection;
import android.media.projection.MediaProjectionManager;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.IBinder;
import android.util.Log;
import android.view.Surface;
import android.view.WindowManager;

import java.nio.ByteBuffer;

/**
 * Sharing the phone's screen: holds the MediaProjection (a foreground
 * service, as Android requires) and encodes the screen with a hardware
 * encoder whose input surface the projection renders into. Encoded frames
 * go to the Rust host (OrielActivity.nativeEncodedFrame).
 */
public class ShareService extends Service {
    static volatile ShareService instance;
    /** The real screen, and the size shared (even, multiple of 16). */
    static volatile int screenWidth, screenHeight, shareWidth, shareHeight;

    private static final String CHANNEL = "share";
    private static final String STOP = "org.fruitiex.oriel.STOP_SHARING";

    private MediaProjection projection;
    private HandlerThread thread;
    private Handler handler;
    private VirtualDisplay display;
    private MediaCodec codec;
    private Surface input;
    private Thread drain;
    private volatile boolean draining;
    private byte[] config;
    private int dpi;

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && STOP.equals(intent.getAction())) {
            stopSelf();
            return START_NOT_STICKY;
        }
        if (projection != null || intent == null) {
            return START_NOT_STICKY;
        }
        NotificationManager nm = getSystemService(NotificationManager.class);
        nm.createNotificationChannel(new NotificationChannel(
                CHANNEL, "Screen sharing", NotificationManager.IMPORTANCE_LOW));
        PendingIntent stop = PendingIntent.getService(this, 0,
                new Intent(this, ShareService.class).setAction(STOP),
                PendingIntent.FLAG_IMMUTABLE);
        Notification n = new Notification.Builder(this, CHANNEL)
                .setContentTitle("oriel is sharing this phone")
                .setContentText("Paired devices can see and control the screen")
                .setSmallIcon(getApplicationInfo().icon)
                .addAction(new Notification.Action.Builder(null, "Stop", stop).build())
                .setOngoing(true)
                .build();
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION);
        } else {
            startForeground(1, n);
        }
        thread = new HandlerThread("oriel-share");
        thread.start();
        handler = new Handler(thread.getLooper());
        MediaProjectionManager mpm = getSystemService(MediaProjectionManager.class);
        projection = mpm.getMediaProjection(
                intent.getIntExtra("code", 0), (Intent) intent.getParcelableExtra("data"));
        if (projection == null) {
            stopSelf();
            return START_NOT_STICKY;
        }
        projection.registerCallback(new MediaProjection.Callback() {
            @Override
            public void onStop() {
                stopSelf();
            }
        }, handler);
        WindowManager wm = getSystemService(WindowManager.class);
        Rect bounds = wm.getMaximumWindowMetrics().getBounds();
        screenWidth = bounds.width();
        screenHeight = bounds.height();
        shareWidth = screenWidth & ~15;
        shareHeight = screenHeight & ~15;
        dpi = getResources().getDisplayMetrics().densityDpi;
        instance = this;
        OrielActivity.nativeShareStarted(shareWidth, shareHeight);
        return START_NOT_STICKY;
    }

    /** Called from Rust (through the activity). */
    synchronized boolean startEncoder(String mime, int width, int height, int fps, int kbps) {
        stopEncoder();
        try {
            MediaFormat format = MediaFormat.createVideoFormat(mime, width, height);
            format.setInteger(MediaFormat.KEY_COLOR_FORMAT,
                    MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface);
            format.setInteger(MediaFormat.KEY_BIT_RATE, kbps * 1000);
            format.setInteger(MediaFormat.KEY_FRAME_RATE, Math.min(fps, 60));
            // Keyframes when the host asks; otherwise rarely.
            format.setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 30);
            // A still screen produces no frames; repeat the last one now and
            // then so the picture sharpens.
            format.setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 200_000);
            format.setInteger(MediaFormat.KEY_PRIORITY, 0);
            format.setInteger(MediaFormat.KEY_LATENCY, 1);
            codec = MediaCodec.createEncoderByType(mime);
            codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE);
            input = codec.createInputSurface();
            codec.start();
            // A projection makes one virtual display (Android 14+); later
            // encoders take it over.
            if (display == null) {
                display = projection.createVirtualDisplay("oriel", width, height, dpi,
                        DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, input, null, handler);
            } else {
                display.resize(width, height, dpi);
                display.setSurface(input);
            }
            config = null;
            draining = true;
            final MediaCodec c = codec;
            drain = new Thread(new Runnable() {
                public void run() {
                    drain(c);
                }
            }, "oriel-encode");
            drain.start();
            return true;
        } catch (Exception e) {
            Log.e("oriel", "cannot start the encoder", e);
            stopEncoder();
            return false;
        }
    }

    private void drain(MediaCodec c) {
        MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
        while (draining) {
            int index;
            try {
                index = c.dequeueOutputBuffer(info, 10_000);
            } catch (IllegalStateException e) {
                return;
            }
            if (index < 0) {
                continue;
            }
            ByteBuffer buffer = c.getOutputBuffer(index);
            byte[] bytes = new byte[info.size];
            if (buffer != null) {
                buffer.position(info.offset);
                buffer.get(bytes);
            }
            c.releaseOutputBuffer(index, false);
            if ((info.flags & MediaCodec.BUFFER_FLAG_CODEC_CONFIG) != 0) {
                config = bytes;
                continue;
            }
            boolean key = (info.flags & MediaCodec.BUFFER_FLAG_KEY_FRAME) != 0;
            if (key && config != null) {
                byte[] both = new byte[config.length + bytes.length];
                System.arraycopy(config, 0, both, 0, config.length);
                System.arraycopy(bytes, 0, both, config.length, bytes.length);
                bytes = both;
            }
            OrielActivity.nativeEncodedFrame(bytes, key, 0);
        }
    }

    synchronized void stopEncoder() {
        draining = false;
        if (drain != null) {
            try {
                drain.join(500);
            } catch (InterruptedException e) {
                // Go on.
            }
            drain = null;
        }
        if (display != null) {
            display.setSurface(null);
        }
        if (codec != null) {
            try {
                codec.stop();
            } catch (Exception e) {
                // Already stopped.
            }
            codec.release();
            codec = null;
        }
        if (input != null) {
            input.release();
            input = null;
        }
    }

    synchronized void requestKeyframe() {
        if (codec != null) {
            Bundle b = new Bundle();
            b.putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0);
            codec.setParameters(b);
        }
    }

    synchronized void setBitrate(int kbps) {
        if (codec != null) {
            Bundle b = new Bundle();
            b.putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, kbps * 1000);
            codec.setParameters(b);
        }
    }

    @Override
    public void onDestroy() {
        stopEncoder();
        if (display != null) {
            display.release();
            display = null;
        }
        if (projection != null) {
            projection.stop();
            projection = null;
        }
        if (thread != null) {
            thread.quitSafely();
        }
        instance = null;
        OrielActivity.nativeShareStopped();
        super.onDestroy();
    }
}
