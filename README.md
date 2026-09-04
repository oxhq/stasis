# Stasis

Stasis is an experimental Servo-based runtime for executing a supported web
application as a controlled event system. The checked-in native and TypeScript package sources
target the 0.3.3 corrective train. They retain frozen `controlled-webapp-v1` and
`controlled-web-session-v1` behavior, with `controlled-web-session-v1` still the default, and add
the separately selected, versioned
[`controlled-web-session-v2` contract](docs/stasis/session-v0.3-candidate.md).

Source version and package CI are not publication proof. `v0.2.1` and
`@oxhq/stasis@0.2.1` remain the last fully qualified predecessor. `v0.3.0` is immutable
disqualified release evidence after its macOS anonymous-consumer failure. The immutable `v0.3.1`
GitHub release is also disqualified: automatic npm prepublication failed in the packed SDK's
cookie-replacement settlement, and `@oxhq/stasis@0.3.1` was never published. The immutable
`v0.3.2` GitHub release is likewise disqualified: its release-event macOS public-package verifier
timed out during cookie-replacement settlement before npm publication, so
`@oxhq/stasis@0.3.2` was never published. Version 0.3.3 is the
stable successor only when its
exact tag, release, registry package, provenance, and anonymous public-consumer evidence exist.
Verify those public artifacts rather than inferring release status from this checkout.

## Linux release prerequisite

The immutable Stasis v0.3.3 Linux x86-64 runtime dynamically loads
`libEGL.so.1` during graphics initialization. On Ubuntu 22.04, install it
before using the release archive directly or through `@oxhq/stasis`:

```sh
sudo apt-get update
sudo apt-get install --yes --no-install-recommends libegl1
```

The v0.3.3 archive's `INSTALL.txt` and `NATIVE-LIBRARIES.txt` omitted this
dynamically loaded prerequisite. The archive and executable bytes are
unchanged. Source builds that run `./mach bootstrap` already install it.

Start with [STASIS.md](STASIS.md) for the product boundary,
[the v0.3 controlled-session contract](docs/stasis/session-v0.3-candidate.md) for the explicit v2
surface, [the frozen v0.2 session contract](docs/stasis/session-v0.2.md) for the default v1 session,
and [the v0.1 protocol](docs/stasis/protocol-v1.md) for the frozen legacy methods. Build and release
operators should also read [the release runbook](docs/stasis/releases.md).

## Servo foundation

Servo is a prototype web browser engine written in the
[Rust](https://github.com/rust-lang/rust) language. It is currently developed on
64-bit macOS, 64-bit Linux, 64-bit Windows, 64-bit OpenHarmony, and Android.

Servo welcomes contribution from everyone. Check out:

- The [Servo Book](https://book.servo.org) for documentation
- [servo.org](https://servo.org/) for news and guides

Coordination of upstream Servo development happens:
- Here in the Github Issues
- On the [Servo Zulip](https://servo.zulipchat.com/)
- In video calls advertised in the [Servo Project](https://github.com/servo/project/issues) repo.

## Getting started

For more detailed build instructions, see the Servo Book under [Getting the Code] and [Building Servo].

[Getting the Code]: https://book.servo.org/building/getting-the-code.html
[Building Servo]: https://book.servo.org/building/building.html

### macOS

- Download and install [Xcode](https://developer.apple.com/xcode/) and [`brew`](https://brew.sh/).
- Install `uv`: `curl -LsSf https://astral.sh/uv/install.sh | sh` 
- Install `rustup`: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- Restart your shell to make sure `cargo` is available
- Install the other dependencies: `./mach bootstrap`
- Build servoshell: `./mach build`

### Linux

- Install `curl`:
  - Arch: `sudo pacman -S --needed curl`
  - Debian, Ubuntu: `sudo apt install curl`
  - Fedora: `sudo dnf install curl`
  - Gentoo: `sudo emerge net-misc/curl`
- Install `uv`: `curl -LsSf https://astral.sh/uv/install.sh | sh` 
- Install `rustup`: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- Restart your shell to make sure `cargo` is available
- Install the other dependencies: `./mach bootstrap`
- Build servoshell: `./mach build`

### Windows

- Download [`uv`](https://docs.astral.sh/uv/getting-started/installation/#standalone-installer), and [`rustup`](https://win.rustup.rs/)
  - Be sure to select *Quick install via the Visual Studio Community installer*
- Ensure that [`winget`](https://learn.microsoft.com/en-us/windows/package-manager/winget/) is available. It should be preinstalled on Windows 10 1809+ and Windows 11, otherwise can be [`manually installed`](https://github.com/microsoft/winget-cli#installing-the-client).
- In the Visual Studio Installer, ensure the following components are installed:
  - **Windows 10/11 SDK (anything >= 10.0.19041.0)** (`Microsoft.VisualStudio.Component.Windows{10, 11}SDK.{>=19041}`)
  - **MSVC v143 - VS 2022 C++ x64/x86 build tools (Latest)** (`Microsoft.VisualStudio.Component.VC.Tools.x86.x64`)
  - **C++ ATL for latest v143 build tools (x86 & x64)** (`Microsoft.VisualStudio.Component.VC.ATL`)
- Restart your shell to make sure `cargo` is available
- Install the other dependencies: `.\mach bootstrap`
- Build servoshell: `.\mach build`

### Android

- Ensure that the following environment variables are set:
  - `ANDROID_SDK_ROOT`
  - `ANDROID_NDK_ROOT`: `$ANDROID_SDK_ROOT/ndk/28.2.13676358/`
 `ANDROID_SDK_ROOT` can be any directory (such as `~/android-sdk`).
  All of the Android build dependencies will be installed there.
- Install the latest version of the [Android command-line
  tools](https://developer.android.com/studio#command-tools) to
  `$ANDROID_SDK_ROOT/cmdline-tools/latest`.
- Run the following command to install the necessary components:
  ```shell
  sudo $ANDROID_SDK_ROOT/cmdline-tools/latest/bin/sdkmanager --install \
   "build-tools;36.0.0" \
   "emulator" \
   "ndk;28.2.13676358" \
   "platform-tools" \
   "platforms;android-37" \
   "system-images;android-37;google_apis;x86_64"
  ```
- Follow the instructions above for the platform you are building on

### OpenHarmony

- Follow the instructions above for the platform you are building on to prepare the environment.
- Depending on the target distribution (e.g. `HarmonyOS NEXT` vs pure `OpenHarmony`) the build configuration will differ slightly.
- Ensure that the following environment variables are set
  - `DEVECO_SDK_HOME` (Required when targeting `HarmonyOS NEXT`)
  - `OHOS_BASE_SDK_HOME` (Required when targeting `OpenHarmony`)
  - `OHOS_SDK_NATIVE` (e.g. `${DEVECO_SDK_HOME}/default/openharmony/native` or `${OHOS_BASE_SDK_HOME}/${API_VERSION}/native`)
  - `SERVO_OHOS_SIGNING_CONFIG`: Path to json file containing a valid signing configuration for the demo app.
- Review the detailed instructions at [Building for OpenHarmony].
- The target distribution can be modified by passing `--flavor=<default|harmonyos>` to `mach <build|package|install>`.
