// Command loadgen drives the performance checks' loads through an engine's
// local proxy (HTTP CONNECT with a node user) to fakenode's echo sink, on
// loopback only:
//
//	stream:   -conns connections sending -rate-mbit in total, paced, for
//	          -duration (unpaced, as fast as they go, when -rate-mbit is
//	          0: throughput); the sink echoes, so as much comes back.
//	          -direction up only sends (to fakenode's discard port as
//	          -target), down only reads (from its source port), unpaced.
//	churn:    -churn-rate new connections per second, each sending and
//	          reading back -churn-bytes, for -duration.
//	pingpong: one connection sending -ping-bytes and reading them back,
//	          again and again for -duration: round-trip times.
//	connect:  -churn-rate new connections per second for -duration, each
//	          timed from the dial until its first byte comes back: the
//	          connection's setup through the proxy and the node.
//
// -direct dials the sink itself, not through the proxy: the baseline the
// proxy's extra latency is measured against.
//
// It prints one JSON line: bytes sent and received, connections that
// completed and that failed, how long it ran, and for pingpong and connect
// the times' p50 and p99 in microseconds.
package main

import (
	"bufio"
	"encoding/base64"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"sort"
	"sync"
	"sync/atomic"
	"time"
)

type result struct {
	BytesSent      int64 `json:"bytes_sent"`
	BytesReceived  int64 `json:"bytes_received"`
	Connections    int64 `json:"connections"`
	FailedConnects int64 `json:"failed_connections"`
	// Echo streams whose entire write was echoed back without a write error.
	// TCP connects alone count even when a capped TUN then resets the stream.
	CompletedEchoes int64 `json:"completed_echoes"`
	// stream: connections where one write made no progress for 10 s (the
	// proxy took none of it); closed then.
	Stalled   int64 `json:"stalled_connections"`
	ElapsedMs int64 `json:"elapsed_ms"`
	Samples   int   `json:"samples,omitempty"`
	P50Us     int64 `json:"p50_us,omitempty"`
	P99Us     int64 `json:"p99_us,omitempty"`
	// connect: the setup's parts, through the proxy: its TCP connection,
	// and the CONNECT's 200 (what follows it until the first byte is the
	// engine's dial to the node and the node's to the sink).
	TCPP50Us int64 `json:"tcp_p50_us,omitempty"`
	OKP50Us  int64 `json:"ok_p50_us,omitempty"`
}

func main() {
	proxy := flag.String("proxy", "", "local proxy host:port")
	user := flag.String("user", "", "proxy username")
	pass := flag.String("pass", "", "proxy password")
	target := flag.String("target", "", "sink host:port")
	mode := flag.String("mode", "stream", "stream or churn")
	conns := flag.Int("conns", 8, "stream: connections")
	rate := flag.Float64("rate-mbit", 50, "stream: total send rate, Mbit/s")
	churnRate := flag.Int("churn-rate", 200, "churn: new connections per second")
	churnBytes := flag.Int("churn-bytes", 4096, "churn: bytes each connection sends and reads back")
	pingBytes := flag.Int("ping-bytes", 64, "pingpong: bytes each round trip")
	direct := flag.Bool("direct", false, "dial the target itself, not through the proxy")
	direction := flag.String("direction", "both", "stream: both (echo), up (send only) or down (read only)")
	duration := flag.Duration("duration", 30*time.Second, "how long the load runs")
	flag.Parse()
	if (*proxy == "" && !*direct) || *target == "" {
		log.Fatal("-proxy (or -direct) and -target are required")
	}
	dial := func() (net.Conn, error) { return connect(*proxy, *user, *pass, *target) }
	if *direct {
		dial = func() (net.Conn, error) { return net.DialTimeout("tcp", *target, 5*time.Second) }
	}
	var r result
	started := time.Now()
	switch *mode {
	case "stream":
		if *direction == "both" {
			stream(dial, *conns, *rate, *duration, &r)
		} else {
			oneWay(dial, *conns, *direction == "up", *duration, &r)
		}
	case "churn":
		churn(dial, *churnRate, *churnBytes, *duration, &r)
	case "pingpong":
		r.setTimes(pingpong(dial, *pingBytes, *duration, &r))
	case "connect":
		timed := func() (net.Conn, time.Duration, time.Duration, error) {
			if *direct {
				conn, err := dial()
				return conn, 0, 0, err
			}
			return connectTimed(*proxy, *user, *pass, *target)
		}
		r.setTimes(setup(timed, *churnRate, *duration, &r))
	default:
		log.Fatalf("unknown mode %q", *mode)
	}
	r.ElapsedMs = time.Since(started).Milliseconds()
	_ = json.NewEncoder(os.Stdout).Encode(r)
	if r.Connections == 0 {
		os.Exit(1)
	}
}

func connect(proxy, user, pass, target string) (net.Conn, error) {
	conn, _, _, err := connectTimed(proxy, user, pass, target)
	return conn, err
}

// connectTimed is connect, saying how long the TCP connection to the proxy
// and the CONNECT's answer took, each from the start.
func connectTimed(proxy, user, pass, target string) (net.Conn, time.Duration, time.Duration, error) {
	start := time.Now()
	conn, err := net.DialTimeout("tcp", proxy, 5*time.Second)
	if err != nil {
		return nil, 0, 0, err
	}
	tcp := time.Since(start)
	auth := base64.StdEncoding.EncodeToString([]byte(user + ":" + pass))
	fmt.Fprintf(conn, "CONNECT %s HTTP/1.1\r\nHost: %s\r\nProxy-Authorization: Basic %s\r\n\r\n", target, target, auth)
	reader := bufio.NewReader(conn)
	response, err := http.ReadResponse(reader, nil)
	if err != nil {
		conn.Close()
		return nil, 0, 0, err
	}
	ok := time.Since(start)
	if response.StatusCode != http.StatusOK {
		conn.Close()
		return nil, 0, 0, fmt.Errorf("CONNECT: %s", response.Status)
	}
	if reader.Buffered() > 0 {
		conn.Close()
		return nil, 0, 0, errors.New("unexpected bytes after CONNECT")
	}
	return conn, tcp, ok, nil
}

func stream(dial func() (net.Conn, error), conns int, rateMbit float64, duration time.Duration, r *result) {
	const chunk = 16 << 10
	// Unpaced when no rate is given: each connection writes as fast as the
	// proxy takes it.
	var interval time.Duration
	if rateMbit > 0 {
		perConn := rateMbit * 1e6 / 8 / float64(conns) // bytes per second
		interval = time.Duration(float64(time.Second) * chunk / perConn)
	}
	deadline := time.Now().Add(duration)
	var wg sync.WaitGroup
	for range conns {
		wg.Add(1)
		go func() {
			defer wg.Done()
			conn, err := dial()
			if err != nil {
				atomic.AddInt64(&r.FailedConnects, 1)
				return
			}
			defer conn.Close()
			atomic.AddInt64(&r.Connections, 1)
			done := make(chan int64, 1)
			go func() {
				n, _ := io.Copy(io.Discard, conn)
				atomic.AddInt64(&r.BytesReceived, n)
				done <- n
			}()
			var sent int64
			writeFailed := false
			buf := make([]byte, chunk)
			var tick <-chan time.Time
			if interval > 0 {
				ticker := time.NewTicker(interval)
				defer ticker.Stop()
				tick = ticker.C
			}
			for time.Now().Before(deadline) {
				// A chunk the proxy takes none of for 10 s: a stalled
				// connection, counted, rather than a run that hangs. A slow
				// link still moves a chunk well within it.
				_ = conn.SetWriteDeadline(time.Now().Add(10 * time.Second))
				n, err := conn.Write(buf)
				sent += int64(n)
				atomic.AddInt64(&r.BytesSent, int64(n))
				if err != nil {
					writeFailed = true
					if errors.Is(err, os.ErrDeadlineExceeded) {
						atomic.AddInt64(&r.Stalled, 1)
					}
					break
				}
				if tick != nil {
					<-tick
				}
			}
			// Let the echo drain, then close.
			if tcp, ok := conn.(*net.TCPConn); ok {
				_ = tcp.CloseWrite()
			}
			select {
			case received := <-done:
				if !writeFailed && sent > 0 && received == sent {
					atomic.AddInt64(&r.CompletedEchoes, 1)
				}
			case <-time.After(5 * time.Second):
			}
		}()
	}
	wg.Wait()
}

func churn(dial func() (net.Conn, error), rate, size int, duration time.Duration, r *result) {
	ticker := time.NewTicker(time.Second / time.Duration(rate))
	defer ticker.Stop()
	deadline := time.Now().Add(duration)
	var wg sync.WaitGroup
	payload := make([]byte, size)
	for time.Now().Before(deadline) {
		<-ticker.C
		wg.Add(1)
		go func() {
			defer wg.Done()
			conn, err := dial()
			if err != nil {
				atomic.AddInt64(&r.FailedConnects, 1)
				return
			}
			defer conn.Close()
			_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
			n, err := conn.Write(payload)
			atomic.AddInt64(&r.BytesSent, int64(n))
			if err != nil {
				atomic.AddInt64(&r.FailedConnects, 1)
				return
			}
			got, err := io.ReadFull(conn, make([]byte, size))
			atomic.AddInt64(&r.BytesReceived, int64(got))
			if err != nil {
				atomic.AddInt64(&r.FailedConnects, 1)
				return
			}
			atomic.AddInt64(&r.Connections, 1)
		}()
	}
	wg.Wait()
}

// pingpong sends size bytes and reads them back, again and again on one
// connection, until the deadline; the round trips' times.
func pingpong(dial func() (net.Conn, error), size int, duration time.Duration, r *result) []time.Duration {
	conn, err := dial()
	if err != nil {
		r.FailedConnects++
		return nil
	}
	defer conn.Close()
	r.Connections++
	if tcp, ok := conn.(*net.TCPConn); ok {
		_ = tcp.SetNoDelay(true)
	}
	out, in := make([]byte, size), make([]byte, size)
	var times []time.Duration
	deadline := time.Now().Add(duration)
	for time.Now().Before(deadline) {
		_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
		start := time.Now()
		if _, err := conn.Write(out); err != nil {
			r.FailedConnects++
			break
		}
		if _, err := io.ReadFull(conn, in); err != nil {
			r.FailedConnects++
			break
		}
		times = append(times, time.Since(start))
		r.BytesSent += int64(size)
		r.BytesReceived += int64(size)
	}
	return times
}

// setup opens rate connections a second until the deadline, each timed
// from the dial until the first byte it sent comes back.
func setup(dial func() (net.Conn, time.Duration, time.Duration, error), rate int, duration time.Duration, r *result) []time.Duration {
	ticker := time.NewTicker(time.Second / time.Duration(rate))
	defer ticker.Stop()
	deadline := time.Now().Add(duration)
	var (
		wg        sync.WaitGroup
		mu        sync.Mutex
		times     []time.Duration
		tcps, oks []time.Duration
	)
	for time.Now().Before(deadline) {
		<-ticker.C
		wg.Add(1)
		go func() {
			defer wg.Done()
			start := time.Now()
			conn, tcp, ok, err := dial()
			if err != nil {
				atomic.AddInt64(&r.FailedConnects, 1)
				return
			}
			defer conn.Close()
			_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
			if _, err := conn.Write([]byte{1}); err != nil {
				atomic.AddInt64(&r.FailedConnects, 1)
				return
			}
			if _, err := io.ReadFull(conn, make([]byte, 1)); err != nil {
				atomic.AddInt64(&r.FailedConnects, 1)
				return
			}
			took := time.Since(start)
			atomic.AddInt64(&r.Connections, 1)
			mu.Lock()
			times = append(times, took)
			if ok > 0 {
				tcps, oks = append(tcps, tcp), append(oks, ok)
			}
			mu.Unlock()
		}()
	}
	wg.Wait()
	r.TCPP50Us, r.OKP50Us = median(tcps), median(oks)
	return times
}

// median of times in microseconds; 0 for none.
func median(times []time.Duration) int64 {
	if len(times) == 0 {
		return 0
	}
	sort.Slice(times, func(i, j int) bool { return times[i] < times[j] })
	return times[len(times)/2].Microseconds()
}

// setTimes records the times' count, p50 and p99.
func (r *result) setTimes(times []time.Duration) {
	if len(times) == 0 {
		return
	}
	sort.Slice(times, func(i, j int) bool { return times[i] < times[j] })
	at := func(q float64) int64 { return times[int(q*float64(len(times)-1))].Microseconds() }
	r.Samples, r.P50Us, r.P99Us = len(times), at(0.50), at(0.99)
}

// oneWay runs conns connections that only send (up) or only read (down),
// unpaced, until the deadline.
func oneWay(dial func() (net.Conn, error), conns int, up bool, duration time.Duration, r *result) {
	deadline := time.Now().Add(duration)
	var wg sync.WaitGroup
	for range conns {
		wg.Add(1)
		go func() {
			defer wg.Done()
			conn, err := dial()
			if err != nil {
				atomic.AddInt64(&r.FailedConnects, 1)
				return
			}
			defer conn.Close()
			atomic.AddInt64(&r.Connections, 1)
			_ = conn.SetDeadline(deadline)
			buf := make([]byte, 32<<10)
			for time.Now().Before(deadline) {
				if up {
					n, err := conn.Write(buf)
					atomic.AddInt64(&r.BytesSent, int64(n))
					if err != nil {
						return
					}
				} else {
					n, err := conn.Read(buf)
					atomic.AddInt64(&r.BytesReceived, int64(n))
					if err != nil {
						return
					}
				}
			}
		}()
	}
	wg.Wait()
}
