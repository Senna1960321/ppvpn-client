// Command fakenode is the performance checks' stand-in node, on loopback
// only: a Shadowsocks 2022 and an AnyTLS server (sing-box, leaving
// directly) and the sinks they connect to: an echo, a discard (reads only:
// upload alone) and a source (writes only: download alone). The AnyTLS certificate is
// generated at start for "localhost" and written to -dir; the engine under
// test trusts it through SSL_CERT_FILE. No key is ever committed.
//
// -redirect sends the node's dials to that range to loopback instead, the
// port kept: a client whose connections a TUN carries dials an address the
// TUN routes (ne_mem.py routes a test range into it), and the node, on the
// same host, would otherwise send them back into the TUN.
//
// It prints one JSON line with the ports and the certificate path, then
// runs until stdin closes or it is signalled.
package main

import (
	"bufio"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/json"
	"encoding/pem"
	"flag"
	"io"
	"log"
	"math/big"
	"net"
	"net/netip"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	box "github.com/sagernet/sing-box"
	C "github.com/sagernet/sing-box/constant"
	"github.com/sagernet/sing-box/include"
	"github.com/sagernet/sing-box/option"
	"github.com/sagernet/sing/common/json/badoption"
)

// SSKey is the Shadowsocks 2022 key and AnyTLSPassword the AnyTLS password
// the performance profile uses: test values, loopback only.
const (
	SSKey          = "AAAAAAAAAAAAAAAAAAAAAA=="
	AnyTLSPassword = "perf-anytls-password"
)

func main() {
	dir := flag.String("dir", "", "directory for the generated certificate (required)")
	redirect := flag.String("redirect", "", "CIDR whose dials go to 127.0.0.1 instead, the port kept")
	localDNS := flag.Bool("local-dns", false, "answer the three benchmark DoT endpoints locally, with verified TLS and 100 ms delay")
	flag.Parse()
	if *dir == "" {
		log.Fatal("-dir is required")
	}
	certPath, keyPath, err := writeCertificate(*dir)
	if err != nil {
		log.Fatal(err)
	}
	var dot *dnsFixture
	if *localDNS {
		dot, err = startDNSFixture(certPath, keyPath, 100*time.Millisecond)
		if err != nil {
			log.Fatal(err)
		}
		defer dot.listener.Close()
	}
	sink, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		log.Fatal(err)
	}
	go serveEcho(sink)
	discard, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		log.Fatal(err)
	}
	go serve(discard, func(conn net.Conn) { _, _ = io.Copy(io.Discard, conn) })
	source, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		log.Fatal(err)
	}
	go serve(source, func(conn net.Conn) {
		buf := make([]byte, 32<<10)
		for {
			if _, err := conn.Write(buf); err != nil {
				return
			}
		}
	})
	ssPort, anytlsPort := freePort(), freePort()
	listen := badoption.Addr(netip.MustParseAddr("127.0.0.1"))
	route := &option.RouteOptions{}
	if dot != nil {
		route.Rules = append(route.Rules, dnsRoute(uint16(dot.listener.Addr().(*net.TCPAddr).Port)))
	}
	if *redirect != "" {
		route.Rules = append(route.Rules, option.Rule{Type: C.RuleTypeDefault, DefaultOptions: option.DefaultRule{
			RawDefaultRule: option.RawDefaultRule{IPCIDR: badoption.Listable[string]{*redirect}},
			RuleAction: option.RuleAction{Action: C.RuleActionTypeRouteOptions,
				RouteOptionsOptions: option.RouteOptionsActionOptions{OverrideAddress: "127.0.0.1"}}}})
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	instance, err := box.New(box.Options{Context: box.Context(ctx, include.InboundRegistry(), include.OutboundRegistry(), include.EndpointRegistry(), include.DNSTransportRegistry(), include.ServiceRegistry()), Options: option.Options{
		Log: &option.LogOptions{Disabled: true},
		Inbounds: []option.Inbound{
			{Type: C.TypeShadowsocks, Tag: "ss", Options: &option.ShadowsocksInboundOptions{
				ListenOptions: option.ListenOptions{Listen: &listen, ListenPort: uint16(ssPort)}, Method: "2022-blake3-aes-128-gcm", Password: SSKey}},
			{Type: C.TypeAnyTLS, Tag: "anytls", Options: &option.AnyTLSInboundOptions{
				ListenOptions:              option.ListenOptions{Listen: &listen, ListenPort: uint16(anytlsPort)},
				Users:                      []option.AnyTLSUser{{Name: "perf", Password: AnyTLSPassword}},
				InboundTLSOptionsContainer: option.InboundTLSOptionsContainer{TLS: &option.InboundTLSOptions{Enabled: true, ServerName: "localhost", CertificatePath: certPath, KeyPath: keyPath}}}},
		},
		Outbounds: []option.Outbound{{Type: C.TypeDirect, Tag: "direct"}},
		Route:     route,
	}})
	if err != nil {
		log.Fatal(err)
	}
	if err := instance.Start(); err != nil {
		log.Fatal(err)
	}
	defer instance.Close()
	_ = json.NewEncoder(os.Stdout).Encode(map[string]any{
		"ss_port": ssPort, "anytls_port": anytlsPort, "sink_port": sink.Addr().(*net.TCPAddr).Port,
		"discard_port": discard.Addr().(*net.TCPAddr).Port, "source_port": source.Addr().(*net.TCPAddr).Port,
		"certificate": certPath,
		"local_dns":   dot != nil, "dns_delay_ms": 100,
	})
	done := make(chan os.Signal, 1)
	signal.Notify(done, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		input := bufio.NewScanner(os.Stdin)
		for input.Scan() {
			if strings.TrimSpace(input.Text()) == "dns-stats" && dot != nil {
				_ = json.NewEncoder(os.Stdout).Encode(dot.stats())
			}
		}
		done <- syscall.SIGTERM
	}()
	<-done
}

// serve runs handle on each connection listener accepts, then closes it.
func serve(listener net.Listener, handle func(net.Conn)) {
	for {
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		go func() {
			defer conn.Close()
			handle(conn)
		}()
	}
}

// serveEcho returns every byte it receives: upload and download of the
// same size.
func serveEcho(listener net.Listener) {
	for {
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		go func() {
			defer conn.Close()
			buf := make([]byte, 32<<10)
			_, _ = io.CopyBuffer(conn, conn, buf)
		}()
	}
}

func freePort() int {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		log.Fatal(err)
	}
	defer listener.Close()
	return listener.Addr().(*net.TCPAddr).Port
}

func writeCertificate(dir string) (string, string, error) {
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return "", "", err
	}
	template := &x509.Certificate{
		SerialNumber: big.NewInt(1), Subject: pkix.Name{CommonName: "localhost"},
		NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(24 * time.Hour),
		DNSNames: []string{"localhost"}, IPAddresses: []net.IP{net.ParseIP("127.0.0.1"), net.ParseIP("1.1.1.1"), net.ParseIP("8.8.8.8"), net.ParseIP("9.9.9.9")},
		KeyUsage: x509.KeyUsageDigitalSignature | x509.KeyUsageCertSign, ExtKeyUsage: []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
		BasicConstraintsValid: true, IsCA: true,
	}
	der, err := x509.CreateCertificate(rand.Reader, template, template, &key.PublicKey, key)
	if err != nil {
		return "", "", err
	}
	keyDER, err := x509.MarshalECPrivateKey(key)
	if err != nil {
		return "", "", err
	}
	certPath, keyPath := filepath.Join(dir, "fakenode.crt"), filepath.Join(dir, "fakenode.key")
	if err := os.WriteFile(certPath, pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}), 0o644); err != nil {
		return "", "", err
	}
	if err := os.WriteFile(keyPath, pem.EncodeToMemory(&pem.Block{Type: "EC PRIVATE KEY", Bytes: keyDER}), 0o600); err != nil {
		return "", "", err
	}
	return certPath, keyPath, nil
}
