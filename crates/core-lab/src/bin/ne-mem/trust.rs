//! A private bundle for the local DoT fixture. Use the same loader as Sail,
//! including SSL_CERT_FILE/SSL_CERT_DIR overrides and native macOS trust.

use std::collections::HashSet;
use std::path::Path;

use btls::x509::X509;

pub(super) fn export_command(args: &[String]) -> Result<(), String> {
    if args.len() != 4 || args[2] != "--extra-ca" {
        return Err("usage: ne-mem --export-trust-bundle <output> --extra-ca <fixture.pem>".into());
    }
    let extra = std::fs::read(&args[3]).map_err(|e| format!("read fixture CA: {e}"))?;
    let native = rustls_native_certs::load_native_certs();
    if !native.errors.is_empty() {
        return Err(format!(
            "cannot load the complete native trust store: {}",
            native.errors[0]
        ));
    }
    let roots: Vec<&[u8]> = native.certs.iter().map(|cert| cert.as_ref()).collect();
    let pem = bundle(&roots, &extra)?;
    write_new(Path::new(&args[1]), &pem)
}

fn write_new(path: &Path, pem: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("create private trust bundle: {e}"))?;
    file.write_all(pem)
        .map_err(|e| format!("write private trust bundle: {e}"))
}

fn bundle(roots: &[&[u8]], extra: &[u8]) -> Result<Vec<u8>, String> {
    if roots.is_empty() {
        return Err(
            "native trust store is empty; refusing to replace it with the fixture CA".into(),
        );
    }
    let fixtures = X509::stack_from_pem(extra).map_err(|e| format!("invalid fixture CA: {e}"))?;
    if fixtures.len() != 1 {
        return Err("fixture must contain exactly one valid PEM certificate".into());
    }
    let mut certs = roots
        .iter()
        .map(|der| X509::from_der(der).map_err(|e| format!("invalid native certificate: {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    certs.extend(fixtures);
    let mut seen = HashSet::new();
    let mut pem = Vec::new();
    for cert in certs {
        let der = cert.to_der().map_err(|e| e.to_string())?;
        if seen.insert(der) {
            pem.extend(cert.to_pem().map_err(|e| e.to_string())?);
        }
    }
    Ok(pem)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn certificate(name: &str) -> X509 {
        use btls::{
            asn1::Asn1Time, hash::MessageDigest, pkey::PKey, rsa::Rsa, x509::X509NameBuilder,
        };
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut subject = X509NameBuilder::new().unwrap();
        subject.append_entry_by_text("CN", name).unwrap();
        let subject = subject.build();
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_subject_name(&subject).unwrap();
        cert.set_issuer_name(&subject).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        cert.build()
    }

    #[test]
    fn keeps_all_native_roots_and_adds_only_the_fixture() {
        let native = certificate("native");
        let extra = certificate("fixture");
        let native_der = native.to_der().unwrap();
        let pem = bundle(&[&native_der, &native_der], &extra.to_pem().unwrap()).unwrap();
        let certs = X509::stack_from_pem(&pem).unwrap();
        assert_eq!(certs.len(), 2);
        assert_eq!(certs[0].to_der().unwrap(), native_der);
        assert_eq!(certs[1].to_der().unwrap(), extra.to_der().unwrap());
    }

    #[test]
    fn refuses_empty_native_roots_and_malformed_fixture() {
        assert!(bundle(&[], b"fixture").unwrap_err().contains("empty"));
        let native = certificate("native").to_der().unwrap();
        for bad in [
            b"".as_slice(),
            b"not a certificate",
            b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----",
        ] {
            assert!(bundle(&[&native], bad).is_err());
        }
    }

    #[test]
    fn export_requires_both_paths_and_does_not_overwrite_files() {
        assert!(export_command(&["--export-trust-bundle".into()]).is_err());
        let path = std::env::temp_dir().join(format!("ne-mem-trust-{}", std::process::id()));
        std::fs::write(&path, b"existing").unwrap();
        assert!(write_new(&path, b"replacement").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
        std::fs::remove_file(path).unwrap();
    }
}
