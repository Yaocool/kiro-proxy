# Kiro Event Stream fixture

`sanitized-kiro-response.aws-eventstream.b64` is a re-encoded compatibility
fixture based on the production shapes recorded by the TypeScript authority and
public Kiro client capture notes. It preserves the production AWS Event Stream
wire shape (all three standard string headers, non-zero prelude and message CRCs,
and assistant/tool/metadata payload shapes) while using inert content,
identifiers, and tool arguments. It is base64 rather than a raw binary file so
reviews and source archives cannot silently alter or drop bytes.

Raw production responses must not be committed because they may contain
credentials, prompt content, repository paths, and model output. When refreshing
this fixture, sanitize payloads first, reassemble the frames with valid CRCs, and
verify the half-frame, sticky-frame, and malformed-frame tests still exercise
the decoder.
