// Interop peer for interop_test.alx: a Go crypto/tls client or server
// using the same test certificates (testdata/certs) and fixed time.
//
//	interop -mode server -addr 127.0.0.1:0 -addrfile F [flags]  accept -n connections
//	interop -mode client -addr 127.0.0.1:PORT [flags]  dial -n times
//
// Each connection: handshake, the client writes "hello <i>\n", the server
// answers "echo hello <i>\n", then both close. Each side prints one line per
// connection: "<i> ver=XXXX suite=XXXX resumed=B alpn=P ekm=HEX".
package main

import (
	"bufio"
	"crypto/tls"
	"crypto/x509"
	"encoding/hex"
	"flag"
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"
)

var (
	mode     = flag.String("mode", "", "client or server")
	addr     = flag.String("addr", "", "address")
	certs    = flag.String("certs", "testdata/certs", "certificate directory")
	cert     = flag.String("cert", "", "own certificate name (empty: none)")
	maxVer   = flag.String("max", "0304", "max version (hex)")
	suite    = flag.String("suite", "", "TLS 1.2 cipher suite (hex), empty: default")
	curve    = flag.Int("curve", 0, "only curve ID (0: default)")
	n        = flag.Int("n", 1, "connections")
	alpn     = flag.String("alpn", "", "ALPN protocol")
	clientCA = flag.Bool("clientauth", false, "server: require and verify a client certificate")
	addrFile = flag.String("addrfile", "", "server: write the listening address to this file")
)

func load(name string) tls.Certificate {
	b, err := os.ReadFile(*certs + "/" + name + ".pem")
	check(err)
	c, err := tls.X509KeyPair(b, b)
	check(err)
	return c
}

func pool(name string) *x509.CertPool {
	p := x509.NewCertPool()
	c := load(name)
	p.AddCert(c.Leaf)
	return p
}

func check(err error) {
	if err != nil {
		fmt.Fprintln(os.Stderr, "interop:", err)
		os.Exit(1)
	}
}

func hex16(s string) uint16 {
	v, err := strconv.ParseUint(s, 16, 16)
	check(err)
	return uint16(v)
}

func config() *tls.Config {
	c := &tls.Config{
		Time:       func() time.Time { return time.Unix(1476984729, 0) },
		MaxVersion: hex16(*maxVer),
		MinVersion: tls.VersionTLS10,
	}
	if *cert != "" {
		c.Certificates = []tls.Certificate{load(*cert)}
	}
	if *suite != "" {
		c.CipherSuites = []uint16{hex16(*suite)}
	}
	if *curve != 0 {
		c.CurvePreferences = []tls.CurveID{tls.CurveID(*curve)}
	}
	if *alpn != "" {
		c.NextProtos = []string{*alpn}
	}
	return c
}

func report(i int, c *tls.Conn) {
	st := c.ConnectionState()
	ekm, err := st.ExportKeyingMaterial("interop", nil, 16)
	check(err)
	fmt.Printf("%d ver=%04X suite=%04X resumed=%v alpn=%s ekm=%s\n", i, st.Version, st.CipherSuite, st.DidResume, st.NegotiatedProtocol, hex.EncodeToString(ekm))
}

func main() {
	flag.Parse()
	cfg := config()
	switch *mode {
	case "server":
		if *clientCA {
			cfg.ClientAuth = tls.RequireAndVerifyClientCert
			cfg.ClientCAs = pool("client_root")
		}
		ln, err := tls.Listen("tcp", *addr, cfg)
		check(err)
		if *addrFile != "" {
			check(os.WriteFile(*addrFile+".tmp", []byte(ln.Addr().String()), 0o644))
			check(os.Rename(*addrFile+".tmp", *addrFile))
		}
		for i := 0; i < *n; i++ {
			raw, err := ln.Accept()
			check(err)
			c := raw.(*tls.Conn)
			check(c.Handshake())
			line, err := bufio.NewReader(c).ReadString('\n')
			check(err)
			_, err = c.Write([]byte("echo " + line))
			check(err)
			report(i, c)
			c.Close()
		}
	case "client":
		cfg.RootCAs = pool("root")
		cfg.ServerName = "test.golang.example"
		cfg.ClientSessionCache = tls.NewLRUClientSessionCache(4)
		for i := 0; i < *n; i++ {
			var c *tls.Conn
			var err error
			for try := 0; try < 100; try++ {
				c, err = tls.Dial("tcp", *addr, cfg)
				if err == nil || !strings.Contains(err.Error(), "refused") {
					break
				}
				time.Sleep(20 * time.Millisecond)
			}
			check(err)
			_, err = fmt.Fprintf(c, "hello %d\n", i)
			check(err)
			line, err := bufio.NewReader(c).ReadString('\n')
			check(err)
			if line != fmt.Sprintf("echo hello %d\n", i) {
				check(fmt.Errorf("bad echo %q", line))
			}
			report(i, c)
			c.Close()
		}
	default:
		check(fmt.Errorf("bad -mode"))
	}
}
