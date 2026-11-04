# Physical device validation

The record of one run of the native iOS host on a physical iPhone: the device,
the build, the commands, and every item of the checklist with the evidence of
the run. The file names no credential, no device identifier and no team
identifier: the device and the team are placeholders, and the evidence is the
output of the commands and of the tests.

## The device and the build

| What | Value |
| --- | --- |
| Date | 2026-11-04 |
| Device | iPhone 12, iOS 18.6.2 |
| Build host | macOS 15.7.2 (Intel), Xcode 26.1.1 (build 17B100) |
| Commit | `10ffde3760197079a6bb4ea9d068d1ea70662fb2` (`chore(release): v0.43.5`) |
| Signing | automatic development signing, a personal team (the team id is not recorded here) |
| Configurations | Release for the install and the launch, Debug for the fixture runs |

The tree on the build host was that commit and nothing else: the commit was
shipped as a bundle, cloned on the host, and `git status --porcelain` in the
checkout was empty.

## The commands

### The build

The signing needs the GUI security session of the console user, because a plain
ssh session cannot use its login keychain and `codesign` fails with
`errSecInternalComponent`. The whole device build runs in that session, with
the complete command inside the wrapper:

```
u=$(id -u); n=$(id -un)
sudo -n launchctl asuser "$u" sudo -n -u "$n" env \
    DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer HOME="/Users/$n" \
    /bin/bash -c 'cd <repo> && xcodebuild -project ios/LegHost.xcodeproj -scheme LegHost \
        -sdk iphoneos -configuration Release -derivedDataPath build/dd-device \
        -destination "generic/platform=iOS" -allowProvisioningUpdates \
        DEVELOPMENT_TEAM=<team-id> build'
```

The install and the launch work from a plain ssh session.

### The install, the launch and the uninstall

```
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
xcrun devicectl device install app --device <device> \
    build/dd-device/Build/Products/Release-iphoneos/LegHost.app
xcrun devicectl device process launch --device <device> com.shukelabs.leg.ioshost
xcrun devicectl device uninstall app --device <device> com.shukelabs.leg.ioshost
```

The launch controls of the host follow a `--`, or `devicectl` reads them as
options of its own:

```
xcrun devicectl device process launch --device <device> \
    com.shukelabs.leg.ioshost -- -leghost-fixture success -leghost-provider anthropic
```

### The tests on the device

The UI tests of the host drive the device the way a person drives the phone:
the touch, the software keyboard, the rotation, the on screen interrupt and the
turn against the loopback fixture.

```
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
xcodebuild test -project ios/LegHost.xcodeproj -scheme LegHost \
    -destination 'platform=iOS,id=<device-udid>' \
    -derivedDataPath build/dd-device
```

## The checklist

Every item was run on the device of the table above on the day of the table.

| Item | Result | The evidence of the run |
| --- | --- | --- |
| The device is attached | pass | `xcrun xctrace list devices` lists the iPhone 12 with its iOS version, and `xcrun devicectl list devices` shows it `connected` |
| The signed `iphoneos` build | pass | the build ends with `** BUILD SUCCEEDED **` and the exit code 0 |
| The install | pass | `devicectl device install app` reports `App installed: bundleID: com.shukelabs.leg.ioshost` and the exit code 0 |
| The launch | pass | `devicectl device process launch` reports `Launched application with com.shukelabs.leg.ioshost bundle identifier`, and `devicectl device info processes` lists the process of the app |
| The first boot imports the guest root | pass | the test `TheFirstLaunchImportsTheGuestRoot` passed on the device, and the container of the app holds `Library/Application Support/LegHost/roots/root.active` |
| Touch and the software keyboard reach the guest | pass | the test `TheTerminalTakesInputAndFollowsTheRotation` typed into the software keyboard of the device and the typed text showed up in the terminal of the guest |
| The terminal shows the output of the guest | pass | the same test and the fixture tests read the output of the guest out of the terminal, and the transcript of the run holds it in `Library/Application Support/LegHost/test-artifacts/terminal-transcript.log` |
| The orientation and the resize | pass | the test `TheTerminalTakesInputAndFollowsTheRotation` saw the size the guest was told about change with the rotation and change back with it |
| The on screen Ctrl-C | pass | the test `AnInterruptEndsTheGuestProgram` tapped the control of the screen and the guest program ended, with no hardware keyboard attached |
| A turn against the loopback HTTP fixture | pass | the test `TheSuccessScenarioCompletesOneTurn` passed: the fixture accepted two requests, both with the status 200, its validation passed, and the terminal holds the text, the tool result and the final response of the turn |
| The interruption and the trail | pass | the test `TheBackgroundInterruptsTheTurnAndKeepsTheTrail` passed: the host showed `IOS_SESSION_INTERRUPTED`, the trail of the stopped turn was kept, and the interrupted turn was not sent again |
| The relaunch keeps the data | pass | the test `TheRelaunchKeepsTheWorkspaceTheTrailAndTheRequests` passed on its own on the device: the relaunch reused the verified root, kept the workspace of the guest, the trail and the durable ledger of the requests, and the turn was not sent again. The container agrees: the session of the guest was still there after a relaunch |
| A runtime failure | pass | the test `TheRuntimeFailureScenarioReportsTheRuntimeDiagnostic` passed: `IOS_GUEST_RUNTIME_ERROR` showed in the terminal and in the status of the host, and no session and no request were made |
| The uninstall and the install clear the guest data | pass | the sessions of the guest catalog went from one to none over the uninstall and the install, and the root was imported again (a new `root.active`) |
| A Release build on a clean device stops at the provider boundary | pass | the container of the Release build holds no guest root: the host read the provider configuration first, found no item in the Keychain of the device, and provisioned nothing |

### The persistence boundaries on the device

The workspace catalog of the guest lives in the guest root, in the container of
the app. The count of its sessions was read out of the container of the device
over the three steps of the run:

| Step | The sessions of the guest |
| --- | --- |
| after a turn that the UI test drove on the device | 1 |
| after a relaunch of the app | 1 |
| after an uninstall and an install | 0 |

### The provider configuration of the device

A build without the DEBUG controls reads the provider configuration from the
Keychain of the device, and the host has no UI to add the item yet (the
settings UI is a later slice). A device without that item therefore stops at
the configuration boundary with `IOS_PROVIDER_CONFIG_MISSING` before it
touches the guest root: nothing is provisioned, booted or started. The
checklist proves that boundary on the device, and it proves the guest path
with the DEBUG build and the loopback fixture, whose credential is the
sentinel `IOS_FIXTURE_CREDENTIAL_SENTINEL` and never leaves the device.

## The tests of the run

The whole UI suite of the host ran on the device, twice:

| Run | `LegHostUITests` | `LegHostFixtureUITests` |
| --- | --- | --- |
| The first run | 5 of 5 passed | 9 of 12 passed |
| The second run | 5 of 5 passed | 11 of 12 passed |

The tests that failed in a run passed when they ran on their own on the
device:

| Test | On its own | In the first run | In the second run |
| --- | --- | --- | --- |
| `testTheProviderErrorScenarioReportsAServerError` | passed | failed | passed |
| `testTheRelaunchKeepsTheWorkspaceTheTrailAndTheRequests` | passed | failed | failed |
| `testTheStoppedGuestCannotWriteAfterManyRetries` | passed | failed | passed |

The failures are the tests that wait for minutes without touching the screen.
In the first run the app was in the background while the test waited: the
hierarchy of the failure holds `IOS_SESSION_INTERRUPTED` and the retry
control, and the host publishes that diagnostic only when the app goes to the
background. The second run was slower on the device (the first test of the
class took 108 s instead of 16 s), and the guest did not reach the composer
within the deadline of the test. Neither is a fault of the guest path: every
one of the tests passed on the device when it ran on its own.

## The checks of the host on the build host

The checks that already existed for the host ran on the build host, against the
same commit:

| Check | Result |
| --- | --- |
| `ios/tests/run-host-tests.sh` (the fixtures of the provisioner) | pass |
| `ios/tests/run-provider-tests.sh` (the fixtures of the provider configuration) | 26 checks, 0 failures |
| `ios/tests/run-lifecycle-tests.sh` (the lifecycle and the preflight tests) | 20 tests, 0 failures |
| `ios/tests/run-fixture-tests.sh` (the fixture UI tests in the simulator) | 11 of 12 passed |

The fixture test that failed in the simulator is
`testTheStoppedGuestCannotWriteAfterManyRetries`, and it failed there with the
accessibility server of the simulator giving up ("failed to get list of
active..."), also when it ran on its own after the simulator was booted again.
The same test passed on the physical device three times: in both runs of the
suite and on its own.

## What the run does not cover

- A turn against a real provider on the device. Every turn of the run went to
  the loopback fixture, because the provider configuration of the device is
  read from the Keychain and the host has no UI to write it yet.
- A device that is locked, or a device without Developer Mode. The run used a
  device that was paired, unlocked and in Developer Mode.
- The signing of a team that is not a personal one. The profiles of a personal
  team expire after seven days, so the build has to be signed again after
  that.
- The behaviour of the host when the device is out of space or when the
  pinned runtime files are broken. The faults of the device are induced by the
  controls of the fixture, and they were checked on the device.
