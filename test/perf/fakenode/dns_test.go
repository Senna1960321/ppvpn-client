package main

import (
	"crypto/tls"
	"crypto/x509"
	"os"
	"reflect"
	"testing"
	"time"

	"github.com/miekg/dns"
)

func fixtureForTest(t *testing.T) (*dnsFixture, *x509.CertPool) {
	t.Helper()
	cert, key, err := writeCertificate(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	fixture, err := startDNSFixture(cert, key, 20*time.Millisecond)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { fixture.listener.Close() })
	pem, err := os.ReadFile(cert)
	if err != nil {
		t.Fatal(err)
	}
	roots := x509.NewCertPool()
	if !roots.AppendCertsFromPEM(pem) {
		t.Fatal("invalid fixture certificate")
	}
	return fixture, roots
}

func TestDNSFixtureVerifiesTLSAndPipelines(t *testing.T) {
	fixture, roots := fixtureForTest(t)
	conn, err := tls.Dial("tcp", fixture.listener.Addr().String(), &tls.Config{RootCAs: roots, ServerName: "1.1.1.1"})
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(3 * time.Second))
	wire := &dns.Conn{Conn: conn}
	const count = 16
	for i := range count {
		request := new(dns.Msg).SetQuestion("load.example.net.", dns.TypeA)
		request.Id = uint16(i)
		if err := wire.WriteMsg(request); err != nil {
			t.Fatal(err)
		}
	}
	seen := make(map[uint16]bool)
	for range count {
		response, err := wire.ReadMsg()
		if err != nil {
			t.Fatal(err)
		}
		if !response.Response || response.Rcode != dns.RcodeNameError || response.Truncated || response.Id >= count || seen[response.Id] {
			t.Fatalf("unexpected response: %v", response)
		}
		if len(response.Question) != 1 || response.Question[0].Name != "load.example.net." {
			t.Fatalf("question changed: %v", response.Question)
		}
		seen[response.Id] = true
	}
	stats := fixture.stats()
	if stats["queries"] != count || stats["connections"] != 1 || stats["peak_inflight"] < 2 {
		t.Fatalf("queries were not pipelined on one TLS connection: %v", stats)
	}
}

func TestDNSFixtureDoesNotDisableCertificateVerification(t *testing.T) {
	fixture, roots := fixtureForTest(t)
	for _, config := range []*tls.Config{
		{RootCAs: x509.NewCertPool(), ServerName: "1.1.1.1"},
		{RootCAs: roots, ServerName: "203.0.113.10"},
	} {
		conn, err := tls.Dial("tcp", fixture.listener.Addr().String(), config)
		if err == nil {
			conn.Close()
			t.Fatal("untrusted certificate or wrong name was accepted")
		}
	}
	for _, name := range []string{"1.1.1.1", "8.8.8.8", "9.9.9.9"} {
		conn, err := tls.Dial("tcp", fixture.listener.Addr().String(), &tls.Config{RootCAs: roots, ServerName: name})
		if err != nil {
			t.Fatal(err)
		}
		conn.Close()
	}
}

func TestDNSRedirectIsOnlyTheConfiguredDoTEndpoints(t *testing.T) {
	rule := dnsRoute(12345).DefaultOptions
	if !reflect.DeepEqual([]string(rule.Network), []string{"tcp"}) ||
		!reflect.DeepEqual([]uint16(rule.Port), []uint16{853}) ||
		!reflect.DeepEqual([]string(rule.IPCIDR), []string{"1.1.1.1/32", "8.8.8.8/32", "9.9.9.9/32"}) {
		t.Fatalf("redirect escaped the three DoT endpoints: %+v", rule.RawDefaultRule)
	}
	if rule.RouteOptionsOptions.OverrideAddress != "127.0.0.1" || rule.RouteOptionsOptions.OverridePort != 12345 {
		t.Fatal("DoT did not target the loopback fixture")
	}
}
