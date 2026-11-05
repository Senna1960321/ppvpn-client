package main

import (
	"crypto/tls"
	"net"
	"sync"
	"sync/atomic"
	"time"

	"github.com/miekg/dns"
	C "github.com/sagernet/sing-box/constant"
	"github.com/sagernet/sing-box/option"
	"github.com/sagernet/sing/common/json/badoption"
)

// Only the benchmark's three configured DoT destinations are redirected.
// In particular, other ports, ordinary DNS and rule-set HTTPS keep their path.
func dnsRoute(port uint16) option.Rule {
	return option.Rule{Type: C.RuleTypeDefault, DefaultOptions: option.DefaultRule{
		RawDefaultRule: option.RawDefaultRule{
			Network: badoption.Listable[string]{"tcp"}, Port: badoption.Listable[uint16]{853},
			IPCIDR: badoption.Listable[string]{"1.1.1.1/32", "8.8.8.8/32", "9.9.9.9/32"},
		},
		RuleAction: option.RuleAction{Action: C.RuleActionTypeRouteOptions,
			RouteOptionsOptions: option.RouteOptionsActionOptions{OverrideAddress: "127.0.0.1", OverridePort: port}},
	}}
}

type dnsFixture struct {
	listener                      net.Listener
	delay                         time.Duration
	connections, queries, replies atomic.Uint64
	inflight, peak                atomic.Int64
}

func startDNSFixture(certPath, keyPath string, delay time.Duration) (*dnsFixture, error) {
	certificate, err := tls.LoadX509KeyPair(certPath, keyPath)
	if err != nil {
		return nil, err
	}
	listener, err := tls.Listen("tcp", "127.0.0.1:0", &tls.Config{
		Certificates: []tls.Certificate{certificate}, MinVersion: tls.VersionTLS12,
	})
	if err != nil {
		return nil, err
	}
	fixture := &dnsFixture{listener: listener, delay: delay}
	go serve(listener, fixture.answer)
	return fixture, nil
}

// Frames are read independently of delayed replies, as a real pipelined DoT
// server. The certificate is verified by the client against a process-only CA.
func (f *dnsFixture) answer(conn net.Conn) {
	f.connections.Add(1)
	wire := &dns.Conn{Conn: conn}
	var writes sync.Mutex
	var replies sync.WaitGroup
	defer replies.Wait()
	// Bound fixture work even if the client stops reading or sends bad input.
	slots := make(chan struct{}, 256)
	for {
		_ = conn.SetReadDeadline(time.Now().Add(time.Minute))
		query, err := wire.ReadMsg()
		if err != nil {
			return
		}
		if query.Response || query.Opcode != dns.OpcodeQuery || len(query.Question) != 1 {
			return
		}
		slots <- struct{}{}
		f.queries.Add(1)
		active := f.inflight.Add(1)
		for peak := f.peak.Load(); active > peak; peak = f.peak.Load() {
			if f.peak.CompareAndSwap(peak, active) {
				break
			}
		}
		replies.Add(1)
		go func(query *dns.Msg) {
			defer replies.Done()
			defer func() { <-slots; f.inflight.Add(-1) }()
			time.Sleep(f.delay)
			response := new(dns.Msg).SetRcode(query, dns.RcodeNameError)
			response.RecursionAvailable = true
			writes.Lock()
			defer writes.Unlock()
			_ = conn.SetWriteDeadline(time.Now().Add(time.Second))
			if wire.WriteMsg(response) == nil {
				f.replies.Add(1)
			}
		}(query)
	}
}

func (f *dnsFixture) stats() map[string]uint64 {
	return map[string]uint64{"connections": f.connections.Load(), "queries": f.queries.Load(),
		"replies": f.replies.Load(), "inflight": uint64(f.inflight.Load()), "peak_inflight": uint64(f.peak.Load())}
}
