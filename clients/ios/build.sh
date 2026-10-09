#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
# SPDX-License-Identifier: AGPL-3.0-or-later

# Build the iOS client: fresh BrookCore xcframework (macOS + iOS device + iOS simulator slices),
# regenerate the Xcode project, build.
#   clients/ios/build.sh        → build for the simulator
#   clients/ios/build.sh test   → check the slices, build for a device with signing off, run the
#                                 unit tests on a simulator, check the required tests ran
#   clients/ios/build.sh run    → build, boot the simulator, install and launch the app. With
#                                 BROOK_ALLOW_INSECURE_HTTP=1 in the environment, the launched
#                                 app gets it too (plain http to a non-loopback address).
# BROOK_IOS_SIMULATOR names the simulator (default "iPhone 17").
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
# This script and clients/macos/build.sh share one output (the xcframework, the generated Swift,
# bindings/apple/build), and the Xcode build reads it after it is written. So each takes the same
# lock for its whole run; the second one waits. The kernel holds the lock, so a killed run
# releases it.
[[ -n "${BROOK_APPLE_LOCKED:-}" ]] || BROOK_APPLE_LOCKED=1 exec /usr/bin/lockf -k "$ROOT/bindings/apple/.build.lock" "$HERE/$(basename "$0")" "$@"

command -v xcodegen >/dev/null || { echo "xcodegen not found (brew install xcodegen)" >&2; exit 1; }
mode="${1:-}"
sim="${BROOK_IOS_SIMULATOR:-iPhone 17}"
derived="$HERE/build.noindex"
sim_dest="platform=iOS Simulator,name=$sim"

"$ROOT/bindings/apple/build-xcframework.sh" --ios
(cd "$HERE" && xcodegen --quiet)

build() {  # build <xcodebuild args...>
  xcodebuild -project "$HERE/Brook.xcodeproj" -scheme Brook -derivedDataPath "$derived" "$@"
}

if [[ "$mode" == "test" ]]; then
  # Tests that must show as passed in the xcodebuild output. A green run alone would not notice
  # a shared test file that silently dropped out of the iOS target. Each step appends its tests.
  REQUIRED_TESTS=(
    SmokeTests/testAppLinksAndCallsTheRustCore
    SmokeTests/testAsyncLoginRunsOnTheRuntimeAndFailsOnTheNetwork
    SmokeTests/testRestoreCallsBackIntoASwiftKeySlot
    SessionPersistenceIOSTests/testFirstLaunchDeletesTheWholeServiceOnceMarksItAndTurnsPersistenceOn
    SessionPersistenceIOSTests/testSecondLaunchDoesNotDeleteAgain
    SessionPersistenceIOSTests/testAFailingDeleteStaysOffLeavesTheMarkerUnsetAndIsRetried
    SessionPersistenceIOSTests/testADataDirectoryThatCannotBeMadeStaysOff
    SessionPersistenceIOSTests/testAFatalProbeStaysOff
    SessionPersistenceIOSTests/testALockedProbeStaysOn
    SessionPersistenceIOSTests/testProtectedDataUnavailableStaysOffWithoutDeletingOrMarking
    SessionPersistenceIOSTests/testAProbeThatFailedButIsNotLockedStaysOffWithoutTouchingAnything
    SessionPersistenceIOSTests/testTheFileProbeSaysAvailableWhenReadable
    SessionPersistenceIOSTests/testTheFileProbeSaysFailedForOtherFailures
    SessionPersistenceIOSTests/testOnlyAPermissionRefusalCountsAsLocked
    SessionPersistenceIOSTests/testAMarkerOnlyInANonPersistentDomainDoesNotCount
    SessionPersistenceIOSTests/testTheCleanupDeletesBeforeAnyReadAndBeforeTheMarker
    SessionPersistenceIOSTests/testOtherDefaultsAlreadySetDoNotSkipTheFirstLaunchCleanup
    MessageWordingTests/testNoMessageNamesTheMac
    MessageWordingTests/testTheIPhoneWording
    SessionStoreTests/testNoFeedFactoryKeepsLocalDataOff
    SessionStoreTests/testALaunchBeforeTheFirstUnlockSaysSoAndStoresNothing
    SessionStoreTests/testWithoutLocalDataSigningOutStillLeavesCoresFence
    ChannelEventsTests/testReadyClearsLiveCalls
    SessionStoreTests/testSignOutSaysItCannotReachTheSavedSignInOnlyInALockedLaunch
    ChannelEventsTests/testReconnectReadyRereadsTheListOnlyWhenAsked
    SessionStoreTests/testSignOutInALockedLaunchLeavesTheNoticeOnTheSignInScreen
    SignedInSessionTests/testStopCancelsTheEventSubscription
    SignedInSessionTests/testSceneChangesReloadThroughTheSession
    SignedInSessionTests/testAReconnectShowsWhatChangedWhileTheSocketWasDown
    ForegroundReloadTests/testComingBackToTheForegroundReadsTheListAgain
    RecoveryWarningTests/testWarnsAtTwoOrFewerAndPluralizes
    ComposerModelTests/testASendClearsAndHandsTheMessageOver
    ComposerModelTests/testAFailedSendGivesTheTextBack
    ComposerModelTests/testANetworkFailureDoesntClaimItWasntSent
    ComposerModelTests/testADirectSendPassesAClientId
    ComposerModelTests/testSendingTheSameTextAgainAfterAFailureReusesItsClientId
    ComposerModelTests/testChangedTextGetsANewClientId
    ComposerModelTests/testChangingTheQuoteGetsANewClientId
    ComposerModelTests/testChangingThenRestoringTheTextGetsANewClientId
    ComposerModelTests/testClearingTheBoxGetsANewClientId
    ComposerModelTests/testStartingAnEditDropsTheDraft
    ComposerModelTests/testDeletingAMessageDropsTheDraft
    ComposerModelTests/testARetryAnsweredWithADeletedMessageIsNotShown
    ComposerModelTests/testTheNextMessageAfterASuccessGetsANewClientId
    TimelineModelTests/testHistoryAndLiveEventsMergeByIdInOrder
    TimelineModelTests/testEditsReplaceAndDeletesStay
    TimelineModelTests/testAnOlderPageAskedWhileTheHeadLoadsWaitsAndEndsAtTheStart
    TimelineModelTests/testAFailedOlderPageIsRetryable
    TimelineModelTests/testAnEmptyOlderPageIsTheStart
    ReadWhenActiveTests/testInTheBackgroundAMessageIsOwedAndReadOnceActive
    ReadWhenActiveTests/testInFrontItsReadAtOnce
    TimelineEventsTests/testReadyRetriesTheOpenTimelinesFailedHead
    TimelineReactionTests/testAnEventForAMessageHereAdjustsItsChips
    TimelineReactionTests/testAResyncForgetsTheOrderingSoALowerSeqIsHeardAgain
    TimelineRereadTests/testWithoutTheFlagAReadyOnlyRetriesAFailedHead
    TimelineRereadTests/testWithTheFlagEveryReadyRereadsTheNewestPage
    TimelineRereadTests/testARereadThatOverlapsMerges
    TimelineRereadTests/testARereadThatDoesNotMeetPagesBackUntilAPageMeets
    TimelineRereadTests/testARereadThatNeverMeetsReplacesAfterThreePagesAndResets
    TimelineRereadTests/testAnOlderPageStartedBeforeAReplacingRereadIsDiscarded
    TimelineRereadTests/testAReadyDuringThePageBackJoinsIt
    TimelineRereadTests/testAPageBackThatFailsKeepsTheShownMessagesAndSaysSo
    TimelineRereadTests/testARereadWhileActiveMarksTheNewestRead
    TimelineRereadTests/testARereadWhileInactiveLeavesTheReadOwed
    TimelineRereadTests/testAReadyClearsTheReactionMarks
    TimelineRereadTests/testAForegroundRereadKeepsTheReactionMarks
    TimelineRereadTests/testAReplaceKeepsMessagesThatArrivedDuringThePageBack
    TimelineRereadTests/testTheGapAnchorIsTakenWhenTheReadyArrives
    TimelineRereadTests/testAnOlderPageWaitingOnAReplacingRereadAsksFromTheNewOldest
    TimelineRereadTests/testAResyncTakesTheGapAnchorWhenItArrives
    TimelineRereadTests/testALiveMessageWithALowerIdThanTheFetchedNewestSurvivesAReplace
    TimelineRereadTests/testALiveEditToAFetchedMessageDuringThePageBackSurvives
    TimelineRereadTests/testARereadAskedDuringAReplacingTurnAsksOnlyTheHeadPage
    TimelineRereadTests/testAFailedRereadKeepsItsAnchorForTheNextOne
    TimelineRereadTests/testATouchedMessageBelowTheFetchedRangeIsNotKeptByAReplace
    ScrollFollowTests/testFollowsANewMessageAtTheBottom
    ScrollFollowTests/testStaysPutForSomeoneElsesMessageWhenAway
    ScrollFollowTests/testFollowsTheUsersOwnMessageEvenWhenAway
  )

  # 1. All three slices are in the xcframework (a Mac build leaves only the macOS one).
  slices="$(ls "$ROOT/bindings/apple/swift/BrookCore/BrookCoreFFI.xcframework")"
  for s in macos-arm64 ios-arm64 ios-arm64-simulator; do
    grep -qx "$s" <<<"$slices" || { echo "xcframework lacks the $s slice" >&2; exit 1; }
  done

  # 2. The real device build (generic iOS) compiles and links; signing is off, so it needs no team.
  build build -destination "generic/platform=iOS" CODE_SIGNING_ALLOWED=NO

  # 3. The unit tests on a simulator. A test that deadlocks must fail, not hang: cap each at 60 s.
  log="$(mktemp)"; trap 'rm -f "$log"' EXIT
  set +e
  # -collect-test-diagnostics never: after a failing test xcodebuild otherwise runs `simctl
  # diagnose`, which collects a simulator log archive and stalls the failure report for ~10
  # minutes. The failure and its message are already in this output, so nothing is lost.
  build test -destination "$sim_dest" -test-timeouts-enabled YES -maximum-test-execution-time-allowance 60 \
    -collect-test-diagnostics never 2>&1 | tee "$log"
  status=${PIPESTATUS[0]}
  set -e
  if (( status != 0 )); then echo "FAIL: xcodebuild test exited $status" >&2; exit "$status"; fi

  # 4. The required tests ran and passed.
  for t in "${REQUIRED_TESTS[@]}"; do
    cls="${t%%/*}"; name="${t##*/}"
    grep -qE "Test Case '-\[[A-Za-z]+\.${cls} ${name}\]' passed" "$log" \
      || { echo "FAIL: required test $t did not pass (or did not run)" >&2; exit 1; }
  done
  echo "ios: all ${#REQUIRED_TESTS[@]} required tests passed"
  exit 0
fi

build build -destination "$sim_dest"

if [[ "$mode" == "run" ]]; then
  app="$derived/Build/Products/Debug-iphonesimulator/Brook.app"
  xcrun simctl boot "$sim" 2>/dev/null || true   # "already booted" is fine
  # Shows the simulator window. Not fatal: some Xcode installs ship no Simulator.app, and the
  # booted device still runs the app (xcrun simctl io booted screenshot shows it).
  open -a Simulator 2>/dev/null || echo "note: Simulator.app not found; the device runs without a window" >&2
  # "booted", not the name: several installed runtimes can share one device name.
  xcrun simctl install booted "$app"
  if [[ "${BROOK_ALLOW_INSECURE_HTTP:-}" == "1" ]]; then
    SIMCTL_CHILD_BROOK_ALLOW_INSECURE_HTTP=1 xcrun simctl launch booted me.madalin.brook
  else
    xcrun simctl launch booted me.madalin.brook
  fi
fi
