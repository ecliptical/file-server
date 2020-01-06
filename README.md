# File Server

Simple HTTP file server with support for conditional fetching.

## Build

`cargo build`

## Run

For help and all supported options: `file-server --help`

E.g., if you have some JSON files in /tmp/files:

`RUST_LOG=debug file-server --listen-addr 127.0.0.1:8080 --ext .json --cache-control max-age=60 /tmp/files`

Then to fetch /tmp/files/foo.json:

```bash
curl -i 'http://localhost:8080/foo'

HTTP/1.1 200 OK
etag: "70bbN1HY6Zk"
last-modified: Mon, 11 Nov 2019 20:51:08 GMT
cache-control: max-age=60
content-type: application/json
content-length: 18
date: Sun, 29 Dec 2019 18:20:04 GMT

{
  "foo": "bar"
}
```

To fetch it conditionally (i.e., only if changed since last fetch):

```bash
curl -i 'http://localhost:8080/foo' -H 'If-None-Match: "70bbN1HY6Zk"'

HTTP/1.1 304 Not Modified
etag: "70bbN1HY6Zk"
last-modified: Mon, 11 Nov 2019 20:51:08 GMT
cache-control: max-age=60
date: Sun, 29 Dec 2019 18:20:04 GMT
```
