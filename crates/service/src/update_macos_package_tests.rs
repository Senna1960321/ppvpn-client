// Real Apple package tools and Security.framework; never runs an installer.
mod macos_package {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::process::Command;

    enum Layout {
        Regular,
        LinkedHelper,
        LinkedDirectory(&'static str),
    }

    struct PackageFixture {
        root: PathBuf,
        current: PathBuf,
        helper: Vec<u8>,
    }

    fn command_succeeds(command: &mut Command) {
        let output = command.output().expect("start fixture tool");
        assert!(
            output.status.success(),
            "{command:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    impl PackageFixture {
        fn new(layout: Layout) -> Self {
            let staging = temp_staging("real-package");
            let root = staging.root.clone();
            let current = staging.current();
            std::fs::create_dir_all(&current).unwrap();
            let outside = root.join("outside");
            std::fs::create_dir(&outside).unwrap();

            // A real, harmless Mach-O executable. Its ad-hoc/unsigned identity
            // must fail the production Developer ID requirement.
            let source = root.join("main.c");
            std::fs::write(&source, "int main(void) { return 0; }\n").unwrap();
            let executable = outside.join("helper");
            command_succeeds(
                Command::new("/usr/bin/cc")
                    .arg(&source)
                    .arg("-o")
                    .arg(&executable),
            );
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
            let helper = std::fs::read(&executable).unwrap();

            let stage = root.join("stage");
            let contents = stage.join("PPVPN.app/Contents");
            let helpers = contents.join("MacOS");
            std::fs::create_dir_all(&helpers).unwrap();
            std::fs::write(
                contents.join("Info.plist"),
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.example.ppvpn.package-test</string>
<key>CFBundleName</key><string>PPVPN</string>
<key>CFBundleExecutable</key><string>PPVPN</string>
<key>CFBundleVersion</key><string>340</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
"#,
            )
            .unwrap();
            for name in ["PPVPN", "ppvpn-service-install", "ppvpn-service"] {
                std::fs::copy(&executable, helpers.join(name)).unwrap();
            }
            match layout {
                Layout::Regular => {}
                Layout::LinkedHelper => {
                    let helper = helpers.join("ppvpn-service-install");
                    std::fs::remove_file(&helper).unwrap();
                    symlink(&executable, &helper).unwrap();
                }
                Layout::LinkedDirectory(name) => {
                    let directory = match name {
                        "MacOS" => &helpers,
                        "Contents" => &contents,
                        _ => panic!("unknown fixture directory"),
                    };
                    let external_directory = outside.join(name);
                    std::fs::rename(directory, &external_directory).unwrap();
                    symlink(external_directory, directory).unwrap();
                }
            }
            command_succeeds(
                Command::new("/usr/bin/pkgbuild")
                    .arg("--root")
                    .arg(&stage)
                    .args([
                        "--install-location",
                        "/Applications",
                        "--identifier",
                        "com.example.ppvpn.package-test",
                        "--version",
                        "340",
                    ])
                    .arg(current.join(PACKAGE)),
            );
            Self {
                root,
                current,
                helper,
            }
        }

        fn prepare_error(&self) -> String {
            // This is deliberately prepare_helpers, not platform::install:
            // neither /usr/sbin/installer nor a package helper may execute.
            platform::prepare_helpers(&self.current, awake() + Duration::from_secs(30))
                .expect_err("unsigned fixture must not be installable")
        }

        fn payload_helpers(&self) -> PathBuf {
            self.current
                .join("payload/Payload/PPVPN.app/Contents/MacOS")
        }

        fn assert_no_helpers_copied(&self) {
            assert_eq!(
                std::fs::read_dir(self.current.join("new")).unwrap().count(),
                0
            );
        }
    }

    impl Drop for PackageFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn real_package_copies_private_payload_bytes_and_mode_then_rejects_signature() {
        let fixture = PackageFixture::new(Layout::Regular);
        let error = fixture.prepare_error();
        assert!(
            error.starts_with("ppvpn-service-install: CLIENT_SIGNATURE_REJECTED:"),
            "{error}"
        );
        let extracted = fixture.payload_helpers().join("ppvpn-service-install");
        let copied = fixture.current.join("new/ppvpn-service-install");
        assert!(std::fs::symlink_metadata(&extracted).unwrap().is_file());
        assert_eq!(std::fs::read(&extracted).unwrap(), fixture.helper);
        assert_eq!(std::fs::read(&copied).unwrap(), fixture.helper);
        assert_eq!(
            std::fs::metadata(copied).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // The first signature failure aborts before staging the next helper.
        assert!(!fixture.current.join("new/ppvpn-service").exists());
    }

    #[test]
    fn real_package_rejects_a_helper_symlink_before_copying_it() {
        let fixture = PackageFixture::new(Layout::LinkedHelper);
        assert_eq!(
            fixture.prepare_error(),
            "ppvpn-service-install: not a regular package file"
        );
        assert!(
            std::fs::symlink_metadata(fixture.payload_helpers().join("ppvpn-service-install"))
                .unwrap()
                .is_symlink()
        );
        fixture.assert_no_helpers_copied();
    }

    #[test]
    fn real_package_rejects_immediate_and_deeper_directory_symlinks() {
        // Contents exercises a non-immediate ancestor: checking only MacOS
        // would follow this link and silently copy bytes outside the payload.
        for directory in ["MacOS", "Contents"] {
            let fixture = PackageFixture::new(Layout::LinkedDirectory(directory));
            assert_eq!(
                fixture.prepare_error(),
                "helper directory is not inside the package payload",
                "{directory}"
            );
            let link = if directory == "MacOS" {
                fixture.payload_helpers()
            } else {
                fixture.payload_helpers().parent().unwrap().to_owned()
            };
            assert!(std::fs::symlink_metadata(link).unwrap().is_symlink());
            fixture.assert_no_helpers_copied();
        }
    }

    #[test]
    fn corrupt_package_stops_before_creating_the_helper_destination() {
        let fixture = PackageFixture::new(Layout::Regular);
        std::fs::write(fixture.current.join(PACKAGE), "not a package").unwrap();
        assert!(fixture.prepare_error().starts_with("pkgutil exit "));
        assert!(!fixture.current.join("new").exists());
    }
}
