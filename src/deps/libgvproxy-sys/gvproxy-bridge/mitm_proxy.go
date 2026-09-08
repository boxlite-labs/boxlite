package main

import (
	"context"
	"crypto/tls"
	"net"
	"net/http"
	"net/http/httputil"
	"time"

	logrus "github.com/sirupsen/logrus"
	"golang.org/x/net/http2"
)

const upstreamDialTimeout = 30 * time.Second

// mitmAndForward handles a MITM'd connection: TLS termination, reverse proxy, secret substitution.
// upstreamTLSConfig overrides the TLS config for upstream connections (nil = system defaults).
func mitmAndForward(guestConn net.Conn, hostname string, destAddr string, ca *BoxCA, secrets []SecretConfig, upstreamTLSConfig ...*tls.Config) {
	cert, err := ca.GenerateHostCert(hostname)
	if err != nil {
		logrus.WithError(err).WithField("hostname", hostname).Error("MITM: cert generation failed")
		guestConn.Close()
		return
	}

	tlsGuest := tls.Server(guestConn, &tls.Config{
		GetCertificate: func(*tls.ClientHelloInfo) (*tls.Certificate, error) {
			return cert, nil
		},
		NextProtos: []string{"h2", "http/1.1"},
	})

	upstreamTransport := &http.Transport{
		ForceAttemptHTTP2: true,
		TLSClientConfig:   resolveUpstreamTLS(hostname, upstreamTLSConfig...),
		DialContext: func(ctx context.Context, network, _ string) (net.Conn, error) {
			return (&net.Dialer{Timeout: upstreamDialTimeout}).DialContext(ctx, network, destAddr)
		},
	}

	proxy := &httputil.ReverseProxy{
		Director: func(req *http.Request) {
			req.URL.Scheme = "https"
			req.URL.Host = hostname
			req.Host = hostname // HTTP/1.1 Host header must match
			// Headers substituted here; body substituted in secretTransport.RoundTrip
			substituteHeaders(req, secrets)
		},
		Transport: &secretTransport{
			inner:   upstreamTransport,
			secrets: secrets,
		},
		FlushInterval: -1,
		ErrorHandler: func(w http.ResponseWriter, r *http.Request, err error) {
			logrus.WithFields(logrus.Fields{
				"hostname": hostname,
				"path":     r.URL.Path,
				"error":    err,
			}).Warn("MITM: upstream error")
			w.WriteHeader(http.StatusBadGateway)
		},
	}

	if err := tlsGuest.Handshake(); err != nil {
		logrus.WithError(err).WithField("hostname", hostname).Debug("MITM: TLS handshake failed")
		guestConn.Close()
		return
	}

	if tlsGuest.ConnectionState().NegotiatedProtocol == "h2" {
		h2srv := &http2.Server{}
		h2srv.ServeConn(tlsGuest, &http2.ServeConnOpts{Handler: proxy})
	} else {
		// HTTP/1.1: use http.Server with a proper shutdown mechanism.
		// After the single connection closes, shut down the server to avoid
		// leaking a goroutine blocked in Accept().
		listener := newSingleConnListener(tlsGuest)
		srv := &http.Server{Handler: proxy}
		srv.Serve(listener) //nolint:errcheck
		// Serve returns when the connection closes — shut down to release resources
		srv.Close()
	}
}

// singleConnListener serves exactly one pre-accepted connection as a net.Listener.
type singleConnListener struct {
	ch     chan net.Conn
	addr   net.Addr
	closed chan struct{}
}

func newSingleConnListener(conn net.Conn) *singleConnListener {
	l := &singleConnListener{
		ch:     make(chan net.Conn, 1),
		addr:   conn.LocalAddr(),
		closed: make(chan struct{}),
	}
	l.ch <- conn
	return l
}

func (l *singleConnListener) Accept() (net.Conn, error) {
	select {
	case conn := <-l.ch:
		return conn, nil
	case <-l.closed:
		return nil, net.ErrClosed
	}
}

func (l *singleConnListener) Close() error {
	select {
	case <-l.closed:
	default:
		close(l.closed)
	}
	return nil
}

func (l *singleConnListener) Addr() net.Addr { return l.addr }

// secretTransport wraps http.RoundTripper to inject streaming body replacement.
type secretTransport struct {
	inner   http.RoundTripper
	secrets []SecretConfig
}

func (t *secretTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	if req.Body != nil && len(t.secrets) > 0 {
		req.Body = newStreamingReplacer(req.Body, t.secrets)
		req.ContentLength = -1
		req.Header.Del("Content-Length")
	}
	return t.inner.RoundTrip(req)
}
