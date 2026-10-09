// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import me.madalin.brook.ui.BrookTheme
import me.madalin.brook.ui.ChannelListScreen
import me.madalin.brook.ui.SignInScreen

/**
 * Only the window: the session and the sign-in form live in [BrookApp], so a rotation recreates
 * this class but loses neither.
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val app = application as BrookApp
        setContent {
            BrookTheme {
                Surface(modifier = Modifier.fillMaxSize()) { Screens(app.session, app.signInForm) }
            }
        }
    }
}

/** The screen is chosen by the session's phase. */
@Composable
private fun Screens(session: SessionModel, form: SignInForm) {
    when (val phase = session.phase.collectAsState().value) {
        Phase.Restoring -> Restoring()
        is Phase.SignedOut, Phase.SigningIn, is Phase.NeedsCode -> SignInScreen(form)
        is Phase.SignedIn -> session.channelList?.let { list ->
            ChannelListScreen(list, phase.user.displayName, onSignOut = session::signOut)
        }
    }
}

@Composable
private fun Restoring() {
    Column(
        modifier = Modifier.fillMaxSize().safeDrawingPadding(),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        CircularProgressIndicator()
        Spacer(Modifier.height(16.dp))
        Text("Signing in...", style = MaterialTheme.typography.bodyLarge)
    }
}
