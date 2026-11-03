package com.peakpassvpn.ppvpn

import android.content.Context
import androidx.work.Constraints
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.Worker
import androidx.work.WorkerParameters

/** Token rotation can arrive after the activity and Dart engine are gone.
 * WorkManager keeps this network job alive and retries transient failures. */
class PushTokenWorker @JvmOverloads constructor(context: Context, params: WorkerParameters,
    private val registerToken: (String, String, String) -> Boolean = { dir, token, bundle -> NativePushToken.register(dir, token, bundle) }
) : Worker(context, params) {
    override fun doWork(): Result {
        val prefs = applicationContext.getSharedPreferences(PushPlugin.PREFS, Context.MODE_PRIVATE)
        val token = prefs.getString(PushPlugin.TOKEN, null) ?: return Result.success()
        val registered = registerToken(applicationContext.filesDir.path, token, applicationContext.packageName)
        // A rotation during the request must not acknowledge the newer token.
        return if (registered && prefs.getString(PushPlugin.TOKEN, null) == token) Result.success() else Result.retry()
    }

    companion object {
        fun enqueue(context: Context) {
            val request = OneTimeWorkRequestBuilder<PushTokenWorker>()
                .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
                .build()
            // KEEP could drop a rotation between the worker's last read and
            // WorkManager marking it complete. Queue it behind active work.
            WorkManager.getInstance(context).enqueueUniqueWork("ppvpn-fcm-registration", ExistingWorkPolicy.APPEND_OR_REPLACE, request)
        }
    }
}

object NativePushToken {
    init { System.loadLibrary("ppvpn_flutter") }
    @JvmStatic external fun register(filesDir: String, token: String, bundleId: String): Boolean
    @JvmStatic external fun recovered(filesDir: String)
}
