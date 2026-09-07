// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package proxy

import (
	"bufio"
	"context"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	apiclient "github.com/boxlite-ai/boxlite/libs/api-client-go"
	common_cache "github.com/boxlite-ai/common-go/pkg/cache"
	common_proxy "github.com/boxlite-ai/common-go/pkg/proxy"
)

type closeWriteConn struct {
	net.Conn
	called bool
}

func (c *closeWriteConn) CloseWrite() error {
	c.called = true
	return nil
}

func TestConnectAuthorityBypassesHTTPRouter(t *testing.T) {
	matched := false
	shutdownWg := &sync.WaitGroup{}
	handler := connectAwareHandler(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
		matched = true
		writer.WriteHeader(http.StatusProxyAuthRequired)
	}), http.NotFoundHandler(), shutdownWg)

	request := httptest.NewRequest(http.MethodConnect, "http://proxy.test", nil)
	request.RequestURI = "proxy.test:443"
	request.URL.Path = ""
	handler.ServeHTTP(httptest.NewRecorder(), request)

	if !matched {
		t.Fatal("authority-form CONNECT did not reach the tunnel handler")
	}
}

func TestConnectHandlerTracksTunnelForShutdown(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	shutdownWg := &sync.WaitGroup{}
	handler := connectAwareHandler(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {
		close(started)
		<-release
	}), http.NotFoundHandler(), shutdownWg)

	request := httptest.NewRequest(http.MethodConnect, "http://proxy.test", nil)
	done := make(chan struct{})
	go func() {
		handler.ServeHTTP(httptest.NewRecorder(), request)
		close(done)
	}()
	<-started

	shutdownDone := make(chan struct{})
	go func() {
		shutdownWg.Wait()
		close(shutdownDone)
	}()
	select {
	case <-shutdownDone:
		t.Fatal("shutdown completed while CONNECT tunnel was active")
	case <-time.After(20 * time.Millisecond):
	}

	close(release)
	select {
	case <-shutdownDone:
	case <-time.After(time.Second):
		t.Fatal("shutdown did not complete after CONNECT tunnel closed")
	}
	<-done
}

func TestTunnelTargetUsesPreviewAuthority(t *testing.T) {
	request := httptest.NewRequest(http.MethodConnect, "http://proxy.test", nil)
	request.Host = "3000-d-416243644566313233343536.proxy.test:443"

	boxID, port, err := (&Proxy{}).tunnelTarget(request)
	if err != nil {
		t.Fatal(err)
	}
	if boxID != "AbCdEf123456" || port != 3000 {
		t.Fatalf("unexpected tunnel target: %s:%d", boxID, port)
	}
}

func TestTunnelConnectRejectsPrivateBoxBeforeRunnerDial(t *testing.T) {
	ctx := context.Background()
	publicCache := common_cache.NewMapCache[bool](ctx)
	if err := publicCache.Set(ctx, "AbCdEf123456", false, time.Minute); err != nil {
		t.Fatal(err)
	}
	proxy := &Proxy{boxPublicCache: publicCache}
	request := httptest.NewRequest(http.MethodConnect, "http://proxy.test", nil)
	request.Host = "3000-d-416243644566313233343536.proxy.test:443"
	response := httptest.NewRecorder()

	proxy.handleTunnelConnect(response, request)

	if response.Code != http.StatusForbidden {
		t.Fatalf("status = %d, want %d", response.Code, http.StatusForbidden)
	}
}

func TestTunnelConnectRenewsBoxActivityBeforeStreaming(t *testing.T) {
	const boxID = "AbCdEf123456"

	renewed := make(chan string, 1)
	api := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		if request.Method == http.MethodPost && strings.HasSuffix(request.URL.Path, "/last-activity") {
			select {
			case renewed <- request.URL.Path:
			default:
			}
			writer.WriteHeader(http.StatusCreated)
			return
		}
		writer.WriteHeader(http.StatusNotFound)
	}))
	defer api.Close()

	ctx := context.Background()
	publicCache := common_cache.NewMapCache[bool](ctx)
	if err := publicCache.Set(ctx, boxID, true, time.Minute); err != nil {
		t.Fatal(err)
	}
	// A runner that refuses the dial: the renewal must already have been sent by
	// the time streaming fails, which is the ordering this test pins.
	runnerCache := common_cache.NewMapCache[RunnerInfo](ctx)
	if err := runnerCache.Set(ctx, boxID, RunnerInfo{ApiUrl: "http://127.0.0.1:1", ApiKey: "runner-key"}, time.Minute); err != nil {
		t.Fatal(err)
	}

	clientConfig := apiclient.NewConfiguration()
	clientConfig.Servers[0].URL = api.URL
	proxy := &Proxy{
		apiclient:                  apiclient.NewAPIClient(clientConfig),
		boxPublicCache:             publicCache,
		boxRunnerCache:             runnerCache,
		boxLastActivityUpdateCache: common_cache.NewMapCache[bool](ctx),
	}

	request := httptest.NewRequest(http.MethodConnect, "http://proxy.test", nil)
	request.Host = "3000-d-416243644566313233343536.proxy.test:443"
	response := httptest.NewRecorder()

	proxy.handleTunnelConnect(response, request)

	select {
	case path := <-renewed:
		if want := "/box/" + boxID + "/last-activity"; path != want {
			t.Fatalf("renewal path = %q, want %q", path, want)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("CONNECT tunnel did not renew the box's last activity")
	}
}

func TestBufferedConnForwardsCloseWrite(t *testing.T) {
	conn, peer := net.Pipe()
	defer conn.Close()
	defer peer.Close()
	tracked := &closeWriteConn{Conn: conn}
	buffered := common_proxy.NewBufferedConn(tracked, bufio.NewReader(conn))

	closeWriter, ok := buffered.(interface{ CloseWrite() error })
	if !ok {
		t.Fatal("buffered connection does not support CloseWrite")
	}
	if err := closeWriter.CloseWrite(); err != nil {
		t.Fatal(err)
	}
	if !tracked.called {
		t.Fatal("CloseWrite was not forwarded")
	}
}
