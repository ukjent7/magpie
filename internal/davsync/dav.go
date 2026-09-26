package davsync

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
)

// folder and file are where the backup lives under the address given.
const (
	folder = "magpie"
	file   = "magpie.magpie-backup"
)

// errChanged is a PUT the server refused because the file changed since it
// was read: another computer synced in between.
var errChanged = errors.New("the file on the server changed meanwhile")

type dav struct {
	base       *url.URL
	user, pass string
	client     *http.Client
}

func newDAV(c Config) (*dav, error) {
	u, err := url.Parse(strings.TrimSpace(c.URL))
	if err != nil || (u.Scheme != "https" && u.Scheme != "http") || u.Host == "" {
		return nil, fmt.Errorf("%q is not a WebDAV address (https://…)", c.URL)
	}
	u.Path = strings.TrimRight(u.Path, "/")
	return &dav{base: u, user: c.User, pass: c.Password, client: http.DefaultClient}, nil
}

func (d *dav) url(parts ...string) string {
	u := *d.base
	u.Path += "/" + strings.Join(parts, "/")
	return u.String()
}

func (d *dav) do(ctx context.Context, method, u string, body []byte, h map[string]string) (*http.Response, error) {
	var r io.Reader
	if body != nil {
		r = bytes.NewReader(body)
	}
	req, err := http.NewRequestWithContext(ctx, method, u, r)
	if err != nil {
		return nil, err
	}
	if d.user != "" || d.pass != "" {
		req.SetBasicAuth(d.user, d.pass)
	}
	for k, v := range h {
		req.Header.Set(k, v)
	}
	res, err := d.client.Do(req)
	if err != nil {
		return nil, err
	}
	if res.StatusCode == http.StatusUnauthorized || res.StatusCode == http.StatusForbidden {
		res.Body.Close()
		return nil, fmt.Errorf("the WebDAV server refused the user name or password (HTTP %d)", res.StatusCode)
	}
	return res, nil
}

// get reads the backup; nil data and no error when there is none yet.
func (d *dav) get(ctx context.Context) (data []byte, etag string, err error) {
	res, err := d.do(ctx, http.MethodGet, d.url(folder, file), nil, nil)
	if err != nil {
		return nil, "", err
	}
	defer res.Body.Close()
	switch {
	case res.StatusCode == http.StatusNotFound || res.StatusCode == http.StatusGone:
		return nil, "", nil
	case res.StatusCode != http.StatusOK:
		return nil, "", fmt.Errorf("reading %s from the WebDAV server: HTTP %d", file, res.StatusCode)
	}
	data, err = io.ReadAll(io.LimitReader(res.Body, 64<<20))
	if err != nil {
		return nil, "", err
	}
	return data, res.Header.Get("ETag"), nil
}

// put writes the backup, only over the version read (etag) when there was
// one, making the folder if the server has none.
func (d *dav) put(ctx context.Context, data []byte, etag string) error {
	h := map[string]string{"Content-Type": "application/octet-stream"}
	if etag != "" {
		h["If-Match"] = etag
	}
	for try := 0; ; try++ {
		res, err := d.do(ctx, http.MethodPut, d.url(folder, file), data, h)
		if err != nil {
			return err
		}
		res.Body.Close()
		switch {
		case res.StatusCode >= 200 && res.StatusCode < 300:
			return nil
		case res.StatusCode == http.StatusPreconditionFailed:
			return errChanged
		case (res.StatusCode == http.StatusNotFound || res.StatusCode == http.StatusConflict) && try == 0:
			if err := d.mkcol(ctx); err != nil {
				return err
			}
			continue
		}
		return fmt.Errorf("writing %s to the WebDAV server: HTTP %d", file, res.StatusCode)
	}
}

func (d *dav) mkcol(ctx context.Context) error {
	res, err := d.do(ctx, "MKCOL", d.url(folder)+"/", nil, nil)
	if err != nil {
		return err
	}
	res.Body.Close()
	// 405: it is there already
	if res.StatusCode >= 300 && res.StatusCode != http.StatusMethodNotAllowed {
		return fmt.Errorf("making the folder %s on the WebDAV server: HTTP %d", folder, res.StatusCode)
	}
	return nil
}
