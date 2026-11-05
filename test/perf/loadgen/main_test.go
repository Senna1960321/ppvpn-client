package main

import (
	"io"
	"net"
	"sync"
	"testing"
	"time"
)

func echoListener(t *testing.T, serve func(net.Conn)) func() (net.Conn, error) {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { listener.Close() })
	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			go func() {
				defer conn.Close()
				serve(conn)
			}()
		}
	}()
	return func() (net.Conn, error) { return net.Dial("tcp", listener.Addr().String()) }
}

func TestStreamCountsCompletedEchoes(t *testing.T) {
	dial := echoListener(t, func(conn net.Conn) { _, _ = io.Copy(conn, conn) })
	var got result
	stream(dial, 4, 8, 100*time.Millisecond, &got)
	if got.Connections != 4 || got.CompletedEchoes != 4 || got.BytesSent != got.BytesReceived {
		t.Fatalf("complete echoes: %+v", got)
	}
}

func TestStreamDoesNotCountAcceptedButResetConnections(t *testing.T) {
	var dialled sync.WaitGroup
	dialled.Add(4)
	dial := echoListener(t, func(conn net.Conn) {
		// An immediate reset can reach Dial before it returns on macOS.
		// Complete all handshakes first so this tests reset streams, not
		// the platform-dependent result of racing a reset with Dial.
		dialled.Wait()
		_, _ = conn.Write([]byte{0})
		_ = conn.(*net.TCPConn).SetLinger(0)
	})
	var got result
	stream(func() (net.Conn, error) {
		conn, err := dial()
		dialled.Done()
		return conn, err
	}, 4, 8, 100*time.Millisecond, &got)
	if got.Connections != 4 || got.CompletedEchoes != 0 {
		t.Fatalf("TCP handshakes are not completed echoes: %+v", got)
	}
}
