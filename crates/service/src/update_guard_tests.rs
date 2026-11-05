// Included in update::tests so the regression tests share its serial lock and
// signed-package fixtures. No service, engine, or installer is started here.
mod guard_tests {
    use super::*;
    use std::sync::{mpsc, Arc};

    struct Fixture(Staging);

    impl Fixture {
        fn new(name: &str) -> Self {
            assert!(
                ACTIVE.lock().is_none(),
                "previous test left an update active"
            );
            Self(temp_staging(name))
        }

        fn job(&self, phase: Phase) -> Job {
            Job {
                id: uuid::Uuid::new_v4().to_string(),
                version: "0.7.5".into(),
                build: 340,
                relaunch: false,
                phase,
                user: None,
                client_app: None,
            }
        }

        fn runner_lock(&self) -> std::fs::File {
            std::fs::create_dir_all(self.0.current()).unwrap();
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(self.0.lock_path())
                .unwrap();
            lock.lock().unwrap();
            lock
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            *ACTIVE.lock() = None;
            let _ = std::fs::remove_dir_all(&self.0.root);
        }
    }

    fn assert_blocked(staging: &Staging) {
        let mut called = false;
        let result = guard_connect_in(Some(staging), || {
            called = true;
            Ok(())
        });
        assert!(!called, "a rejected Connect must not reach CoreManager");
        assert_eq!(result.unwrap_err().to_string(), IN_PROGRESS);
    }

    #[test]
    fn connect_does_not_enter_core_during_any_active_update_phase() {
        let _serial = serial();
        let fixture = Fixture::new("connect-active");
        for phase in [
            Phase::Checking,
            Phase::Waiting,
            Phase::Installing,
            Phase::Finished,
        ] {
            *ACTIVE.lock() = Some(fixture.job(phase));
            assert_blocked(&fixture.0);
        }
        *ACTIVE.lock() = None;
        assert_eq!(guard_connect_in(Some(&fixture.0), || Ok(17)).unwrap(), 17);
    }

    #[test]
    fn detached_runner_blocks_connect_after_restart_until_its_lock_is_released() {
        let _serial = serial();
        let fixture = Fixture::new("connect-runner");
        let lock = fixture.runner_lock();
        write_job(&fixture.0, &fixture.job(Phase::Installing)).unwrap();
        assert!(
            ACTIVE.lock().is_none(),
            "the restarted service has no local job"
        );
        assert_blocked(&fixture.0);

        // A status record is not the exclusion primitive. Even a missing or
        // finished record must not let Connect bypass a still-running worker.
        std::fs::remove_file(fixture.0.job_path()).unwrap();
        assert_blocked(&fixture.0);
        write_job(&fixture.0, &fixture.job(Phase::Finished)).unwrap();
        assert_blocked(&fixture.0);

        drop(lock);
        assert_eq!(
            guard_connect_in(Some(&fixture.0), || Ok("connected")).unwrap(),
            "connected"
        );
    }

    #[test]
    fn connect_holds_the_update_reservation_through_core_start() {
        let _serial = serial();
        let fixture = Fixture::new("connect-race");
        let signer = Signer::new();
        let key = signer.key();
        let payload = download(&fixture.0.root.join("cache"), &signer, b"installer");
        let core = Arc::new(Mutex::new(false));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let connect_staging = fixture.0.clone();
        let connect_core = core.clone();
        let connecting = std::thread::spawn(move || {
            guard_connect_in(Some(&connect_staging), || {
                // This has the same lock order as IPC Connect: update, CORE.
                let mut running = connect_core.lock();
                *running = true;
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(23)
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        // Directly observe the reservation while the core starts. This does
        // not depend on the competing thread getting scheduled within a sleep.
        let reservation_held = ACTIVE.try_lock().is_none();
        let check_staging = fixture.0.clone();
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let checking = std::thread::spawn(move || {
            attempt_tx.send(()).unwrap();
            check_in(
                check_staging,
                &key,
                332,
                payload,
                &|| *core.lock(),
                &good_checks(),
            )
            .err()
        });
        attempt_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        release_tx.send(()).unwrap();
        let connect_result = connecting.join().unwrap();
        let check_result = checking.join().unwrap();

        assert!(
            reservation_held,
            "Connect released the update guard before starting its core"
        );
        assert_eq!(connect_result.unwrap(), 23);
        assert_eq!(
            check_result,
            Some(BUSY),
            "the later update must see the new core"
        );
        assert!(ACTIVE.lock().is_none());
        assert!(
            !fixture.0.current().exists(),
            "a busy update must not stage a package"
        );
    }

    #[test]
    fn cancelling_a_staged_update_and_a_failed_connect_both_release_the_guard() {
        let _serial = serial();
        let fixture = Fixture::new("connect-release");
        let signer = Signer::new();
        let payload = download(&fixture.0.root.join("cache"), &signer, b"installer");
        let staged = check_with(&fixture.0, &signer, payload, &good_checks()).unwrap();
        assert_blocked(&fixture.0);
        staged.discard("client cancelled before acceptance");
        assert!(!fixture.0.current().exists());

        let error = guard_connect_in(Some(&fixture.0), || -> anyhow::Result<()> {
            anyhow::bail!("core refused its profile")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "core refused its profile");
        assert_eq!(guard_connect_in(Some(&fixture.0), || Ok(29)).unwrap(), 29);
        assert!(ACTIVE.lock().is_none());
    }

    #[test]
    fn an_unreadable_runner_lock_fails_closed_but_an_absent_staging_root_is_idle() {
        let _serial = serial();
        let fixture = Fixture::new("connect-lock-error");
        assert_eq!(guard_connect_in(Some(&fixture.0), || Ok(31)).unwrap(), 31);

        // Opening a directory for writing fails on both supported platforms.
        // An I/O error is not evidence that the installer is absent.
        std::fs::create_dir_all(fixture.0.lock_path()).unwrap();
        let mut called = false;
        let result = guard_connect_in(Some(&fixture.0), || {
            called = true;
            Ok(())
        });
        assert!(result.is_err());
        assert!(!called, "an indeterminate lock state must not start a core");
    }

    #[test]
    fn connect_keeps_the_disk_reservation_through_core_start() {
        let _serial = serial();
        let fixture = Fixture::new("connect-disk-reservation");
        std::fs::create_dir_all(&fixture.0.root).unwrap();
        guard_connect_in(Some(&fixture.0), || {
            let error = reservation(&fixture.0).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
            Ok(())
        })
        .unwrap();
        assert!(reservation(&fixture.0).unwrap().is_some());
    }

    #[test]
    fn a_late_runner_cannot_install_after_restart_has_allowed_a_connection() {
        let _serial = serial();
        let fixture = Fixture::new("late-runner-connect");
        std::fs::create_dir_all(fixture.0.current()).unwrap();
        let job = fixture.job(Phase::Installing);
        write_job(&fixture.0, &job).unwrap();
        // The service died after spawning, before the worker claimed its job.
        guard_connect_in(Some(&fixture.0), || {
            assert!(!fixture.0.job_path().exists());
            assert!(claim_runner(&fixture.0, &job.id).is_err());
            Ok(())
        })
        .unwrap();
        assert!(claim_runner(&fixture.0, &job.id).is_err());
        assert_eq!(
            read_result(&fixture.0).unwrap().error.as_deref(),
            Some("UPDATE_INSTALL_FAILED:interrupted")
        );
    }

    #[test]
    fn cleanup_cannot_remove_a_claimed_runners_job_or_lock() {
        let _serial = serial();
        let fixture = Fixture::new("claimed-runner-cleanup");
        std::fs::create_dir_all(fixture.0.current()).unwrap();
        let job = fixture.job(Phase::Installing);
        write_job(&fixture.0, &job).unwrap();
        let (lock, _) = claim_runner(&fixture.0, &job.id).unwrap();
        assert!(settle(&fixture.0).is_err());
        assert_eq!(read_job(&fixture.0), Some(job.clone()));
        assert!(fixture.0.lock_path().is_file());
        assert_blocked(&fixture.0);
        drop(lock);
        settle(&fixture.0).unwrap();
        assert!(
            fixture.0.lock_path().is_file(),
            "cleanup unlinked the reservation"
        );
        assert!(!fixture.0.current().exists());
        assert!(claim_runner(&fixture.0, &job.id).is_err());
    }

    #[test]
    fn a_late_runner_cannot_claim_a_new_job_in_the_same_directory() {
        let _serial = serial();
        let fixture = Fixture::new("late-runner-new-job");
        std::fs::create_dir_all(fixture.0.current()).unwrap();
        let old = fixture.job(Phase::Installing);
        write_job(&fixture.0, &old).unwrap();
        settle(&fixture.0).unwrap();
        std::fs::create_dir_all(fixture.0.current()).unwrap();
        let new = fixture.job(Phase::Installing);
        assert_ne!(old.id, new.id);
        write_job(&fixture.0, &new).unwrap();
        assert!(claim_runner(&fixture.0, &old.id).is_err());
        assert_eq!(claim_runner(&fixture.0, &new.id).unwrap().1, new);
    }

    #[test]
    fn cleanup_does_not_claim_an_install_was_interrupted_while_waiting_for_the_app() {
        let _serial = serial();
        let fixture = Fixture::new("interrupted-waiting");
        std::fs::create_dir_all(fixture.0.current()).unwrap();
        let job = fixture.job(Phase::Waiting);
        write_job(&fixture.0, &job).unwrap();
        settle(&fixture.0).unwrap();
        assert!(read_result(&fixture.0).is_none());
        assert!(claim_runner(&fixture.0, &job.id).is_err());
    }

    #[test]
    fn a_missing_job_id_cannot_claim_a_legacy_or_new_job() {
        let _serial = serial();
        let fixture = Fixture::new("missing-runner-id");
        std::fs::create_dir_all(fixture.0.current()).unwrap();
        let mut job = fixture.job(Phase::Installing);
        job.id.clear();
        write_job(&fixture.0, &job).unwrap();
        assert!(claim_runner(&fixture.0, "").is_err());
        assert!(claim_runner(&fixture.0, "unrelated").is_err());
    }

    #[test]
    fn a_lock_error_must_not_clean_up_or_replace_the_pending_job() {
        let _serial = serial();
        let fixture = Fixture::new("settle-lock-error");
        std::fs::create_dir_all(fixture.0.current()).unwrap();
        std::fs::create_dir(fixture.0.lock_path()).unwrap();
        let job = fixture.job(Phase::Installing);
        write_job(&fixture.0, &job).unwrap();
        assert!(settle(&fixture.0).is_err());
        assert_blocked(&fixture.0);
        assert_eq!(read_job(&fixture.0), Some(job));
        assert!(read_result(&fixture.0).is_none());
    }
}
