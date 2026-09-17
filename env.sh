# Toolchain for the Android target. Source it:  . ./env.sh
#
# The system JDK on this machine is 25, which Gradle 8.13 / AGP 8.7 refuse. A
# JDK 21 lives beside the SDK for that reason - do not "simplify" this back to
# the system java.
export ANDROID_HOME="$HOME/Android/Sdk"
export ANDROID_SDK_ROOT="$ANDROID_HOME"
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/26.3.11579264"   # r26d
export ANDROID_NDK="$ANDROID_NDK_HOME"
export JAVA_HOME="$HOME/Android/jdk21"

export PATH="$JAVA_HOME/bin:$ANDROID_HOME/platform-tools:$ANDROID_HOME/emulator:$ANDROID_HOME/cmdline-tools/latest/bin:$HOME/.cargo/bin:$PATH"
