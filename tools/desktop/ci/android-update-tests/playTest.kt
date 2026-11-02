package com.peakpassvpn.ppvpn

import android.app.Activity
import android.content.pm.PackageManager
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class UpdatePluginTest {
    @Test fun playBuildCannotCheckDownloadOrRequestInstallPermission() {
        val activity = Robolectric.buildActivity(Activity::class.java).setup().get()
        val channel = ChannelHarness()
        val plugin = UpdatePlugin(activity, channel)
        try {
            assertEquals("play", BuildConfig.FLAVOR)
            assertEquals("", BuildConfig.PPVPN_UPDATE_FEED)
            assertEquals(mapOf("available" to false), channel.await(channel.call("state")))
            for (method in listOf("check", "install")) {
                assertSame(ChannelHarness.NotImplemented, channel.await(channel.call(method)))
            }
            val info = activity.packageManager.getPackageInfo(activity.packageName, PackageManager.GET_PERMISSIONS)
            assertFalse(info.requestedPermissions.orEmpty().contains("android.permission.REQUEST_INSTALL_PACKAGES"))
            assertNull(activity.packageManager.resolveContentProvider("${activity.packageName}.updates", 0))
        } finally { plugin.close() }
        assertNull(channel.handler)
    }
}
