//! SQL Server's binary XML, decoded back to text.
use tsql_server::binxml::to_xml;

fn name(s: &str) -> Vec<u8> {
    let mut v = vec![0xF0, s.encode_utf16().count() as u8];
    v.extend(s.encode_utf16().flat_map(|u| u.to_le_bytes()));
    v
}

fn text(s: &str) -> Vec<u8> {
    let mut v = vec![0x11, s.encode_utf16().count() as u8];
    v.extend(s.encode_utf16().flat_map(|u| u.to_le_bytes()));
    v
}

const HEAD: [u8; 5] = [0xDF, 0xFF, 0x01, 0xB0, 0x04];

#[test]
fn untyped_elements_attributes_and_text() {
    let mut b = HEAD.to_vec();
    b.extend(name("root"));
    b.extend([0xEF, 0, 0, 1, 0xF8, 1]); // <root
    b.extend(name("id"));
    b.extend([0xEF, 0, 0, 2, 0xF6, 2]);
    b.extend(text("a\"b"));
    b.extend([0xF5]); // attributes end
    b.extend(name("child"));
    b.extend([0xEF, 0, 0, 3, 0xF8, 3]);
    b.extend(text("x < y & z"));
    b.extend([0xF7]); // </child>
    b.extend(name("empty"));
    b.extend([0xEF, 0, 0, 4, 0xF8, 4, 0xF7]);
    b.extend([0xF7]); // </root>
    assert_eq!(to_xml(&b).unwrap(), r#"<root id="a&quot;b"><child>x &lt; y &amp; z</child><empty/></root>"#);
}

#[test]
fn namespaces_come_from_xmlns_attributes() {
    let mut b = HEAD.to_vec();
    b.extend(name("doc"));
    b.extend(name("urn:x"));
    b.extend([0xEF, 2, 0, 1, 0xF8, 1]);
    b.extend(name("xmlns"));
    b.extend([0xEF, 0, 3, 0, 0xF6, 2]);
    b.extend(text("urn:x"));
    b.extend([0xF5, 0xF7]);
    assert_eq!(to_xml(&b).unwrap(), r#"<doc xmlns="urn:x"></doc>"#);
}

fn typed(blob_type: u8, storage: u8, value: &[u8]) -> Vec<u8> {
    let mut v = vec![0xEA, 5, 0, blob_type, 0, 0, storage];
    v.extend(value);
    v
}

#[test]
fn typed_values() {
    let mut head = vec![0xDF, 0xFF, 0x02, 0xB0, 0x04, 0xEA, 5, 0, 0x2A, 0, 1, 0];
    head.extend(name("v"));
    head.extend([0xEF, 0, 0, 1, 0xF8, 1]);
    let mut decimal = |neg: bool, mant: u128, scale: u8| {
        let mut b = head.clone();
        let mut val = vec![0x87, 0x13, 38, scale, if neg { 0 } else { 1 }];
        val.extend(mant.to_le_bytes());
        b.extend(typed(0x13, 0x13, &val));
        b.push(0xF7);
        to_xml(&b).unwrap()
    };
    assert_eq!(decimal(false, 800_000 * 10u128.pow(10), 10), "<v>800000</v>");
    assert_eq!(decimal(true, 2899 * 10u128.pow(8), 10), "<v>-28.99</v>");
    assert_eq!(decimal(false, 0, 10), "<v>0</v>");
    // a string, and a date with the offset 0
    let mut b = head.clone();
    b.extend(typed(0x0F, 0x0F, &text("M")));
    b.push(0xF7);
    assert_eq!(to_xml(&b).unwrap(), "<v>M</v>");
    let mut b = head.clone();
    let mut date = vec![0x7C, 7, 0, 0, 0, 0, 0];
    date.extend([0x42, 0x29, 0x0B, 0, 0]); // 731458 days after 0001-01-01
    b.extend(typed(0x17, 0x17, &date));
    b.push(0xF7);
    assert_eq!(to_xml(&b).unwrap(), "<v>2003-09-01Z</v>");
    // a real
    let mut b = head.clone();
    let mut real = vec![3];
    real.extend(21000f32.to_le_bytes());
    b.extend(typed(0x11, 0x11, &real));
    b.push(0xF7);
    assert_eq!(to_xml(&b).unwrap(), "<v>21000</v>");
}

#[test]
fn what_is_not_known_is_an_error_not_a_guess() {
    let mut b = vec![0xDF, 0xFF, 0x02, 0xB0, 0x04];
    b.extend(name("v"));
    b.extend([0xEF, 0, 0, 1, 0xF8, 1]);
    b.extend(typed(0x55, 0x55, &[1, 2, 3]));
    assert!(to_xml(&b).unwrap_err().contains("not known"));
    assert!(to_xml(&[1, 2, 3]).is_err());
    let mut c = HEAD.to_vec();
    c.extend([0xF8, 9]); // a name that was never defined
    assert!(to_xml(&c).is_err());
    let mut d = HEAD.to_vec();
    d.extend(name("a"));
    d.extend([0xEF, 0, 0, 1, 0xF8, 1]); // never closed
    assert!(to_xml(&d).is_err());
    assert!(to_xml(&[0xDF, 0xFF, 1, 0xB0, 4, 0xF0, 200]).is_err(), "a string that runs past the end");
}
