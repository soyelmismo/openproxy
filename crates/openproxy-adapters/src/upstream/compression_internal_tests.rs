#[cfg(test)]
mod codec_tests {
    use crate::upstream::compression::*;
    use crate::upstream::error::UpstreamError;
    use http::{HeaderMap, HeaderValue, header};

    fn encoded(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Ok(header) = HeaderValue::from_str(value) {
            headers.insert(header::CONTENT_ENCODING, header);
        }
        headers
    }

    #[test]
    fn absent_content_encoding_is_passthrough() {
        assert_eq!(response_codec(&HeaderMap::new()).ok(), Some(None));
    }

    #[test]
    fn gzip_is_recognized_case_insensitively_and_via_x_gzip() {
        for value in ["gzip", "GZIP", "x-gzip"] {
            assert_eq!(
                response_codec(&encoded(value)).ok(),
                Some(Some(Codec::Gzip)),
                "{value}"
            );
        }
    }

    #[test]
    fn deflate_is_recognized() {
        assert_eq!(
            response_codec(&encoded("Deflate")).ok(),
            Some(Some(Codec::Deflate))
        );
    }

    #[test]
    fn identity_is_passthrough() {
        assert_eq!(response_codec(&encoded("identity")).ok(), Some(None));
    }

    #[test]
    fn unsupported_codec_fails_instead_of_handing_raw_bytes_to_parsers() {
        for value in ["compress", "gzip, deflate"] {
            assert!(
                matches!(
                    response_codec(&encoded(value)),
                    Err(UpstreamError::Decode(_))
                ),
                "{value}"
            );
        }
    }

    #[test]
    fn negotiation_is_skipped_when_caller_named_one() {
        let mut headers = HeaderMap::new();
        negotiate_request(&mut headers, false);
        assert_eq!(
            headers
                .get(header::ACCEPT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some(ADVERTISED_ACCEPT_ENCODING)
        );

        for explicit in ["identity", "gzip", "br, zstd", "gzip;q=0"] {
            let mut headers = HeaderMap::new();
            if let Ok(header) = HeaderValue::from_str(explicit) {
                headers.insert(header::ACCEPT_ENCODING, header);
            }
            negotiate_request(&mut headers, false);
            assert_eq!(
                headers
                    .get(header::ACCEPT_ENCODING)
                    .and_then(|v| v.to_str().ok()),
                Some(explicit),
                "{explicit}"
            );
        }
    }

    #[test]
    fn streaming_requests_never_advertise_one_shot_codecs() {
        // `br`/`zstd` decode only at EOF: an upstream that answered them on a
        // stream would have the whole response buffered here, so streaming
        // must be limited to the incrementally-decoded codecs.
        for is_streaming in [true, false] {
            let mut headers = HeaderMap::new();
            negotiate_request(&mut headers, is_streaming);
            let advertised = headers
                .get(header::ACCEPT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .expect("accept-encoding set");

            assert_eq!(
                advertised,
                if is_streaming {
                    INCREMENTAL_ACCEPT_ENCODING
                } else {
                    ADVERTISED_ACCEPT_ENCODING
                },
                "is_streaming={is_streaming}"
            );
        }
    }

    #[test]
    fn built_hyper_request_carries_the_streaming_scoped_codecs() {
        use crate::upstream::UpstreamRequest;
        use crate::upstream::client::build_hyper_request;

        for (is_streaming, expected) in [
            (true, INCREMENTAL_ACCEPT_ENCODING),
            (false, ADVERTISED_ACCEPT_ENCODING),
        ] {
            let mut spec = UpstreamRequest::get("https://example.invalid/v1/messages");
            spec.is_streaming = is_streaming;
            let request = build_hyper_request(spec).expect("request builds");
            let advertised = request
                .headers()
                .get(header::ACCEPT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .expect("accept-encoding negotiated on the wire request");

            assert_eq!(advertised, expected, "is_streaming={is_streaming}");
        }
    }

    #[test]
    fn decoded_response_drops_the_wire_format_headers() {
        let mut headers = encoded("gzip");
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("123"));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let decoding = BodyDecoding::from_response(&headers).expect("decoder");
        assert!(decoding.is_decoding());
        decoding.strip_decoded_headers(&mut headers);

        assert!(headers.get(header::CONTENT_ENCODING).is_none());
        assert!(headers.get(header::CONTENT_LENGTH).is_none());
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn plain_response_keeps_every_header() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("7"));
        let decoding = BodyDecoding::from_response(&headers).expect("decoder");
        assert!(!decoding.is_decoding());
        decoding.strip_decoded_headers(&mut headers);
        assert_eq!(
            headers
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some("7")
        );
    }

    #[test]
    fn passthrough_frames_are_byte_identical() {
        let mut decoding = BodyDecoding::default();
        decoding.push(b"hello").expect("push");
        assert_eq!(decoding.take_frame().as_deref(), Some(&b"hello"[..]));
        assert!(decoding.take_frame().is_none());
    }

    #[test]
    fn empty_body_with_content_encoding_is_not_a_decode_error() {
        let mut decoding = BodyDecoding::from_response(&encoded("gzip")).expect("decoder");
        assert!(decoding.finish().is_ok());
    }
}

#[cfg(test)]
mod gzip_header_tests {
    use crate::upstream::compression::MAX_GZIP_HEADER_BYTES;
    use crate::upstream::compression::compression_gzip::*;
    use crate::upstream::error::UpstreamError;

    fn fixed(flg: u8) -> Vec<u8> {
        vec![0x1f, 0x8b, 0x08, flg, 0, 0, 0, 0, 0x00, 0xff]
    }

    #[test]
    fn minimal_header_puts_body_at_ten() {
        let mut buf = fixed(0x00);
        buf.extend_from_slice(&[0xaa, 0xbb]);
        assert_eq!(gzip_body_offset(&buf).ok(), Some(Some(10)));
    }

    #[test]
    fn all_optional_fields_are_walked() {
        // FLG = FHCRC | FEXTRA | FNAME | FCOMMENT
        let mut buf = fixed(0x1e);
        buf.extend_from_slice(&[0x04, 0x00]); // XLEN = 4
        buf.extend_from_slice(b"ABCD"); // FEXTRA
        buf.push(b'n');
        buf.push(0); // FNAME
        buf.push(b'c');
        buf.push(0); // FCOMMENT
        // FHCRC is verified, so it must be the real low 16 bits of the CRC32
        // over the header up to and not including this field.
        let fhcrc = u16::try_from(crc32fast::hash(&buf) & 0xffff).unwrap_or_default();
        buf.extend_from_slice(&fhcrc.to_le_bytes());
        let body_at = buf.len();
        buf.push(0x99); // first body byte
        assert_eq!(gzip_body_offset(&buf).ok(), Some(Some(body_at)));
    }

    #[test]
    fn partial_optional_field_waits_for_more_input() {
        let mut buf = fixed(0x08); // FNAME
        buf.extend_from_slice(b"unterminated");
        assert_eq!(gzip_body_offset(&buf).ok(), Some(None));
    }

    #[test]
    fn incomplete_fixed_header_waits() {
        assert_eq!(gzip_body_offset(&[]).ok(), Some(None));
        assert_eq!(gzip_body_offset(&[0x1f, 0x8b, 0x08]).ok(), Some(None));
    }

    #[test]
    fn bad_magic_and_method_are_rejected() {
        let mut buf = fixed(0x00);
        assert!(gzip_body_offset(&buf).is_ok());
        if let Some(byte) = buf.get_mut(1) {
            *byte = 0x8c;
        }
        assert!(matches!(
            gzip_body_offset(&buf),
            Err(UpstreamError::Decode(_))
        ));
        if let Some(byte) = buf.get_mut(1) {
            *byte = 0x8b;
        }
        if let Some(byte) = buf.get_mut(2) {
            *byte = 9;
        }
        assert!(matches!(
            gzip_body_offset(&buf),
            Err(UpstreamError::Decode(_))
        ));
    }

    #[test]
    fn unterminated_fcomment_is_bounded() {
        let mut buf = fixed(0x10); // FCOMMENT
        buf.resize(MAX_GZIP_HEADER_BYTES + 1, b'x');
        assert!(matches!(
            gzip_body_offset(&buf),
            Err(UpstreamError::Decode(_))
        ));
    }

    #[test]
    fn zlib_sniff_accepts_only_valid_headers() {
        assert!(looks_like_zlib(&[0x78, 0x9c]));
        assert!(looks_like_zlib(&[0x78, 0x01]));
        // CMF low nibble must be 8.
        assert!(!looks_like_zlib(&[0x79, 0x01]));
        // (CMF<<8|FLG) % 31 must be 0.
        assert!(!looks_like_zlib(&[0x78, 0x9d]));
        // Window size must be <= 7.
        assert!(!looks_like_zlib(&[0xf8, 0x00]));
        assert!(!looks_like_zlib(&[0x78]));
    }
}
