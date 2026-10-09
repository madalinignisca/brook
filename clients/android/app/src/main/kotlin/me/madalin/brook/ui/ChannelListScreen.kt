// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.unit.dp
import me.madalin.brook.ChannelListModel
import me.madalin.brook.ConversationRow
import me.madalin.brook.ListState

/**
 * The conversation list under a top bar with the account menu. Rows are not clickable: opening
 * a conversation comes with the feature that shows messages.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChannelListScreen(model: ChannelListModel, displayName: String, onSignOut: () -> Unit) {
    val state by model.state.collectAsState()
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Brook") },
                actions = { AccountMenu(displayName, onSignOut) },
            )
        },
    ) { padding ->
        when (val now = state) {
            ListState.Loading -> Centered(padding) { CircularProgressIndicator() }
            ListState.Error -> Centered(padding) {
                Text("Couldn't load your conversations.")
                Button(onClick = model::retry) { Text("Try again") }
            }
            is ListState.Loaded ->
                if (now.channels.isEmpty() && now.dms.isEmpty()) {
                    Centered(padding) { Text("No conversations yet") }
                } else {
                    Conversations(now, padding)
                }
        }
    }
}

@Composable
private fun Centered(padding: PaddingValues, content: @Composable () -> Unit) {
    Column(
        modifier = Modifier.fillMaxSize().padding(padding),
        verticalArrangement = Arrangement.spacedBy(12.dp, Alignment.CenterVertically),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) { content() }
}

@Composable
private fun Conversations(state: ListState.Loaded, padding: PaddingValues) {
    LazyColumn(contentPadding = padding, modifier = Modifier.fillMaxSize()) {
        // A section with no rows has no header either.
        if (state.channels.isNotEmpty()) {
            item(key = "header-channels") { Header("Channels") }
            items(state.channels, key = { it.id }) { Row(it) }
        }
        if (state.dms.isNotEmpty()) {
            item(key = "header-dms") { Header("Direct messages") }
            items(state.dms, key = { it.id }) { Row(it) }
        }
    }
}

@Composable
private fun Header(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 16.dp, bottom = 4.dp),
    )
}

@Composable
private fun Row(row: ConversationRow) {
    ListItem(
        headlineContent = { Text(row.label) },
        leadingContent = { Avatar(row) },
    )
}

/** A round letter. Its color comes from the theme and depends only on the id, so it never changes. */
@Composable
private fun Avatar(row: ConversationRow) {
    val colors = MaterialTheme.colorScheme
    // `String.hashCode` is specified (not per-run), so a conversation keeps its color across launches.
    val (background, foreground) = when (Math.floorMod(row.id.hashCode(), 3)) {
        0 -> colors.primaryContainer to colors.onPrimaryContainer
        1 -> colors.secondaryContainer to colors.onSecondaryContainer
        else -> colors.tertiaryContainer to colors.onTertiaryContainer
    }
    Box(
        modifier = Modifier.size(40.dp).clip(CircleShape).background(background),
        contentAlignment = Alignment.Center,
    ) {
        // The label's first letter; a channel's "#" is not one.
        val letter = row.label.firstOrNull { it.isLetterOrDigit() } ?: row.label.firstOrNull() ?: '?'
        Text(letter.uppercase(), color = foreground, style = MaterialTheme.typography.titleMedium)
    }
}

/** The signed-in user's avatar in the bar opens a menu with their name and "Sign out". */
@Composable
private fun AccountMenu(displayName: String, onSignOut: () -> Unit) {
    var open by remember { mutableStateOf(false) }
    Box(modifier = Modifier.padding(end = 8.dp)) {
        Box(modifier = Modifier.clickable { open = true }) {
            Avatar(ConversationRow(id = displayName, label = displayName))
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            DropdownMenuItem(text = { Text(displayName) }, onClick = {}, enabled = false)
            DropdownMenuItem(text = { Text("Sign out") }, onClick = {
                open = false
                onSignOut()
            })
        }
    }
}
