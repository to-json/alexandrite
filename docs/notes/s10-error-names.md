# S10: std error texts name alx's API

Decision (GO-VS-RUBY S10, user 2026-10-05): std's error and panic messages
name alx's functions, methods, fields and constants, not Go's. Behaviour
follows current Go; only the names change. A port adjusts the expectations
it takes from Go; it doesn't translate alx's names back into Go's spelling.

## The rule

- A message names the API as alx declares it, with its `!` / `?` suffix
  (`bytes.Reader.unread_byte!`, `probably_prime?`). Fields are snake_case
  (`server_name`), constants CAPS (`slog.TIME_KEY`, `MODE_DIR`). Go's `()`
  after a method without arguments is dropped (`opts.hash_func`).
- Type names alx shares with Go stay (`Reader`, `Scanner`, `Value`,
  `StartElement`, `RWMutex`, `FileHeader`).
- Go's own words stay where they are protocol or byte-for-byte output (see
  "Kept").

## Translations removed

- fmt (scan.alx): `go_strconv` turned `strconv.parse_float:` into
  `strconv.ParseFloat:` in scan errors. Gone: `strconv.parse_float: parsing
  "1e500": value out of range`.
- log/slog (level.alx): `parse_level` replaced `strconv.atoi` by
  `strconv.Atoi`. Gone.

## Messages renamed (old → new)

- bytes: `bytes.Reader.ReadAt/UnreadByte/UnreadRune/Seek:` →
  `read_at` / `unread_byte!` / `unread_rune!` / `seek!`; "previous operation
  was not ReadRune" → `read_rune!`.
- crypto/cipher: `cipher.NewOFB/NewCTR/NewCBCEncrypter/NewCBCDecrypter:` →
  `new_ofb` / `new_ctr` / `new_cbc_encrypter` / `new_cbc_decrypter`;
  `NewGCM requires` → `new_gcm`.
- crypto/subtle: `subtle.XORBytes:` → `subtle.xor_bytes:`.
- compress/gzip: `gzip.Write: Extra data is too large` / `non-Latin-1
  header string` → `gzip.Writer.write!: extra data is too large` / `...:
  non-Latin-1 header string`.
- net/rpc: `rpc.Register:` → `rpc.register:`.
- database/sql: `Register called twice` → `register`; `Scan error on column
  index` → `scan error`; `destination arguments in Scan` → `scan`;
  `unsupported Scan, storing` → `unsupported scan`; `Scan called without
  calling Next` → `scan ... next`; `Tx.Stmt:` → `Tx.stmt:`; driver's `no
  LastInsertId` / `no RowsAffected` → `last_insert_id` / `rows_affected`.
- crypto/ecdsa: `Sign must be called` → `sign`; `from SignASN1` →
  `sign_asn1`; curve-not-supported texts name `PublicKey.bytes`,
  `PrivateKey.bytes`, `parse_uncompressed_public_key`,
  `parse_raw_private_key`.
- crypto/sha3: `Write after Read` → `write! after read!`; `Sum after Read` →
  `sum after read!`.
- crypto/internal/fips140/edwards25519: `SetUniformBytes` /
  `SetBytesWithClamping` → `set_uniform_bytes` / `set_bytes_with_clamping`.
- crypto/elliptic: `ScalarMult was called` → `scalar_mult`.
- crypto/internal/fips140/bigmod: `ExpShortVarTime` → `exp_short_var_time!`.
- crypto/rsa: `GenerateMultiPrimeKey:` → `generate_multi_prime_key:`.
- crypto/x509: the "use ParseX instead" hints → `parse_ec_private_key`,
  `parse_pkcs8_private_key`, `parse_pkcs1_private_key`,
  `parse_pkix_public_key`, `parse_pkcs1_public_key`; `use ParseCertificate`
  → `parse_certificate`; `template.ThisUpdate is after template.NextUpdate`
  → `template.this_update ... template.next_update`; `zero RevocationTime
  field` → `revocation_time`; the ReasonCode extra-extension text →
  `reason code in extra_extensions; use the reason_code field`; `MaxPathLen`
  → `max_path_len`; `requested SignatureAlgorithm does not match` →
  `signature_algorithm`; `PrivateKey doesn't match parent's PublicKey` →
  `private key ... public_key`.
- crypto/tls: `ServerName` / `InsecureSkipVerify` → `server_name` /
  `insecure_skip_verify`; `NextProtos` → `next_protos`; `MinVersion and
  MaxVersion` → `min_version and max_version`; `neither Certificates,
  GetCertificate, nor GetConfigForClient set in Config` → `neither
  certificates nor get_certificate set in Config` (alx has no
  get_config_for_client); `KeyLogWriter:` → `key_log_writer:`;
  `ExportKeyingMaterial` → `export_keying_material`; `CloseWrite called` →
  `close_write!`; `VerifyHostname called` → `verify_hostname`.
- crypto/ed25519: `opts.HashFunc() zero` → `opts.hash_func zero`; `opts.Hash
  zero` → `opts.hash zero`.
- encoding/xml: `EncodeToken of` → `encode_token! of`; `EncodeElement of` →
  `encode_element! of`; `.MarshalXML wrote` → `.marshal_xml`;
  `.UnmarshalXML did not consume` → `.unmarshal_xml!`;
  `Decoder.CharsetReader is nil` → `Decoder.charset_reader`; `cannot use
  RawToken from UnmarshalXML method` → `raw_token! ... unmarshal_xml!`;
  `ProcInst with invalid Target` → `invalid target`.
- html/template, text/template: `cannot Parse after Execute` → `cannot parse
  after execute`; `cannot Clone` → `cannot clone`; `call to ParseFiles` →
  `parse_files`.
- math/big: `Float.SetFloat64(NaN)` → `Float.set_float64!(NaN)`;
  `NewFloat(NaN)` → `new_float(NaN)`; `GobDecode:` / `GobEncode:` →
  `gob_decode!:` / `gob_encode:`; `Rat.Scan:` / `Int.Scan:` → `scan!:`;
  `Int.Jacobi` → `big.jacobi`; `negative n for ProbablyPrime` →
  `probably_prime?`.
- time: `Time.MarshalText:` → `Time.marshal_text:`.
- mime/multipart: `SetBoundary called` → `set_boundary!`; `NextPart:` →
  `next_part!:`; `unexpected line in Next()` → `in next_part!`.
- archive/zip: `FileHeader.Name/Extra too long` → `FileHeader.name/extra`;
  `Writer.Comment too long` → `Writer.comment`; `SetOffset called` →
  `set_offset!`.
- golang.org/x/crypto: chacha20 `SetCounter` → `set_counter!`; poly1305
  `after Sum or Verify` → `after sum! or verify!`; chacha20poly1305 `passed
  to Seal/Open` → `seal/open`.
- math/rand/v2: `invalid argument to Int64N/Uint64N/Int32N/Uint32N/IntN/UintN`
  → `int64_n!` / `uint64_n!` / `int32_n!` / `uint32_n!` / `int_n!` /
  `uint_n!`.
- net/netip: `ParseAddr("x"): ...` → `parse_addr("x"): ...`;
  `netip.ParsePrefix(` → `netip.parse_prefix(` (net/url's host errors embed
  these).
- regexp: `regexp: Compile(` / `CompilePOSIX(` panics → `compile(` /
  `compile_posix(`.
- encoding/csv: `passed to FieldPos` → `field_pos`.
- net/http: httptest `invalid NewRequest arguments` → `new_request`;
  `invalid WriteHeader code` → `write_header!`; `http: ContentLength=...
  with Body length` → `content_length=... with body length`;
  `Request.ContentLength=... with nil Body` → `Request.content_length=...
  with no body`; `in Request.URL` → `Request.url`; `invalid
  Cookie.Name/Expires/Value/Path/Domain` → snake_case fields.
- testing: the stop message `(FailNow or SkipNow)` → `(fail_now or
  skip_now)`; `B.Loop called with timer stopped` → `B.loop`; `without
  B.Loop() == false` → `without B.loop == false`.
- testing/fstest: every Go method name in its failure texts (`open`,
  `read_dir!` on files, `fsys.read_dir` / `fs.read_dir`, `read_file`, `stat`,
  `lstat`, `glob`, `fs.sub`, `close!`, `read_all`, `entry.info`, `is_dir`,
  `type`, `MODE_DIR`, snake_case entry labels, `test_fs found errors`,
  `failed test_reader`).
- testing/iotest: `Read(` → `read!(`, `ReadAll` → `read_all`, `Seek(` →
  `seek!(`.
- testing/slogtest explanations: `slog.TimeKey, slog.LevelKey and
  slog.MessageKey` → `slog.TIME_KEY, slog.LEVEL_KEY and slog.MESSAGE_KEY`;
  `Record.Time` → `Record.time`; `WithAttrs` / `WithGroup` → `with_attrs` /
  `with_group`; `call Resolve` → `call resolve`; `not output SourceKey if
  the PC is zero` → `not output SOURCE_KEY if the record has no source`.
  The case names ("WithAttrs", "empty-PC") are Go's and stay.
- log/slog: `LogValue called too many times` → `log_value`.
- io: `multiple Read calls return no data or error` → `multiple read!
  calls ...`.
- io/fs: `inner fsys Glob:` → `inner fsys glob:`.
- sync: `Unlock` / `RUnlock of unlocked RWMutex` → `unlock` / `runlock`.
- net/smtp: `Hello called after other methods` → `hello! called`.

## Kept (Go's words on purpose)

- Protocol and wire text: HTTP status lines and cookie attributes
  (`HttpOnly`, `SameSite=`); TLS handshake message names (ClientHello,
  ServerHello, HelloRetryRequest, ServerKeyExchange, ...), signature-context
  strings and scheme names; ASN.1 type names (PrintableString,
  GeneralizedTime, ...); SQL isolation-level names; tar format names
  (USTAR/PAX/GNU); fs.PathError op words (`readlink`, `readdir`, `glob`);
  JSON/XML syntax error texts.
- Output that reproduces Go's byte for byte: `%!v(big.Int=...)`,
  `*rsa.PublicKey`-style key type names, gob type names (`*big.Rat`), json
  v2 Go type names, `jsontext.Value`, template content types
  (`template.HTML`), `reflect.Value` texts in templates, `json: error
  calling MarshalJSON` (the protocol text Go's encoder and html/template
  write).
- Concepts alx names the same way or that have no alx spelling:
  `crypto.Signer`, `crypto.Decrypter`, `crypto.Hash(0)`,
  `json.MarshalerTo` / `UnmarshalerFrom`, `MarshalEncode`,
  `errors.ErrUnsupported`, `gob: GobDecoder:`, `asn1.Flag`, `SignerOpts`,
  `CurveIDs`, `bufio.Scanner: token too long`.
- Internal errors (`p256AffineTable.Select`, `nistec ScalarBaseMult`,
  `unexpected InstFail`).

## Generated vectors

These vec files were edited by hand for the renames; their generators
(outside the repo) need the same renames, or regenerating brings Go's
spelling back: fmt/vec_scan_test, log/slog/vec_level_test,
net/netip/vec_netip_test, net/url/vec_url_test,
mime/multipart/vec_multipart_test, net/http/httputil/vec_dump_test,
crypto/x509/vec_x509_keys_test, crypto/x509/vec_x509_create_test,
encoding/xml/vec_token_test.

## Behaviour that follows current Go

- encoding/json writes invalid UTF-8 in a string as a raw U+FFFD, as Go
  1.27's (v2-backed) encoding/json does; the older v1 encoder wrote the
  escape `�` (port issue #96).
