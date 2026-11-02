package com.peakpassvpn.ppvpn

import android.os.Looper
import io.flutter.plugin.common.BinaryMessenger
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.StandardMethodCodec
import java.nio.ByteBuffer
import java.util.concurrent.CompletableFuture
import org.robolectric.Shadows.shadowOf

/** Real Flutter method codec with an in-process messenger, no Flutter engine. */
class ChannelHarness : BinaryMessenger {
    var handler: BinaryMessenger.BinaryMessageHandler? = null
    override fun send(channel: String, message: ByteBuffer?) = error("Unexpected outgoing message")
    override fun send(channel: String, message: ByteBuffer?, callback: BinaryMessenger.BinaryReply?) =
        error("Unexpected outgoing message")
    override fun setMessageHandler(channel: String, handler: BinaryMessenger.BinaryMessageHandler?) {
        check(channel == "ppvpn/update")
        this.handler = handler
    }

    fun call(method: String, arguments: Any? = null): CompletableFuture<Any?> {
        val future = CompletableFuture<Any?>()
        val bytes = StandardMethodCodec.INSTANCE.encodeMethodCall(MethodCall(method, arguments))
        bytes.flip()
        checkNotNull(handler).onMessage(bytes) { envelope ->
            if (envelope == null) future.complete(NotImplemented)
            else {
                envelope.flip()
                try { future.complete(StandardMethodCodec.INSTANCE.decodeEnvelope(envelope)) }
                catch (error: Throwable) { future.completeExceptionally(error) }
            }
        }
        return future
    }

    fun await(result: CompletableFuture<Any?>): Any? {
        val until = System.nanoTime() + 5_000_000_000L
        while (!result.isDone && System.nanoTime() < until) {
            shadowOf(Looper.getMainLooper()).idle()
            Thread.sleep(2)
        }
        check(result.isDone) { "Method channel result was never completed" }
        return result.get()
    }

    object NotImplemented
}
