# Android update tests

The workflow predicate test uses GitHub's expression evaluator against the
actual release workflow:

```sh
npm --prefix tools/desktop/ci/android-update-tests ci --ignore-scripts
npm --prefix tools/desktop/ci/android-update-tests test
```

The native runner needs JDK 17 or newer, Gradle 9.3.1, Android SDK platform
and build tools 36, command-line tools (`apkanalyzer`), and Flutter's Android
embedding jar (`flutter precache --android`). Set `JAVA_HOME` and
`ANDROID_HOME`, then run from the repository root:

```sh
python3 tools/desktop/ci/android-update-tests/run.py \
  --gradle /path/to/gradle \
  --flutter-root /path/to/flutter \
  --work /path/to/temporary/android-update-tests
```

CI uses the generated Flutter project's Gradle wrapper after its Android
build. The runner generates a separate test project under `--work`, applies
the production flavor generator, and copies the complete APK and Play source
sets, including manifests and resources. Robolectric runs the real updater
and Flutter method codec on API 26 and 35. HTTPS responses and Android package
metadata are controlled test fixtures.

The signature checks use AGP's compiled manifest/resource APK, temporary test
keys, and real `apksigner`, `keytool`, and `apkanalyzer` commands. They cover
v2-only APK acceptance, debug/unsigned rejection, the AAB helper's JAR
signature branch, and the production APK metadata packager. No test key or
generated artifact is published.

These tests do not replace a device check of the installer, system permission
screen, background networking, or the complete signed upgrade experience.
