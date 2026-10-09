// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import android.app.Application

/**
 * The process-wide object. Empty for now: the manifest names it so the app has one place to own
 * long-lived state (the session model, later steps) that must outlive any single Activity.
 */
class BrookApp : Application()
