// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import uniffi.brook_ffi.conversationLabel

/**
 * Proves the whole chain works: this call goes through JNA into libbrook_ffi.so (core, built by
 * Gradle's RustLibs task). If the library failed to load, the app would crash at launch, so
 * seeing "#general" on screen is the proof. Later steps replace this with the real screens.
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                Text(conversationLabel("channel", "general", emptyList(), "me", false))
            }
        }
    }
}
