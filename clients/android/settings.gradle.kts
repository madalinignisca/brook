// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

pluginManagement {
    // Plugins (AGP, the Compose compiler) come from Google's and Maven Central's repositories only.
    repositories {
        google()
        mavenCentral()
    }
}

dependencyResolutionManagement {
    // Every dependency is declared here, once; a module adding its own repository would be an
    // unreviewed place to download code from, so that fails the build.
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "brook-android"
include(":app")
