use std::collections::HashMap;
use std::fmt::{Debug, Display, Formatter};
use std::ascii;
use std::ascii::escape_default;
use itertools::join;
use itertools::Itertools;

#[derive(Debug, PartialEq, Clone)]
pub enum BEncodeNode {
    Integer(isize),
    ByteString(Vec<u8>),
    List(Vec<BEncodeNode>),
    Dict(HashMap<Vec<u8>, BEncodeNode>),
}

#[derive(Debug, PartialEq)]
pub enum BEncodeNodeParseError {
    DanglingEndMarker, // To be returned when we encounter 'e' as the first character in a Node. THIS VARIANT IS NOT ALWAYS AN ERROR; parsing a list or dict will expect this to happen at the end of the inner contents.
    InvalidASCIIInToken, // For when a BENCODE control token (which is presumed to be valid ASCII) turns out not to be.
    UnexpectedEnd,  // For when we are looking for a sentinel character (usually 'e') but the input string terminates abruptly.
    ExcessInput,  // For when the input string extends past the end of a single valid, parsable Node.
    MalformedInteger,  // Failure to parse a valid ASCII string as an integer.
    InvalidDictKey,  // When parsing a dictionary, the keys must always be Nodes of type ByteString.
    Other,
}

impl std::ops::Index<&[u8]> for BEncodeNode {
    type Output = BEncodeNode;
    fn index(&self, index: &[u8]) -> &Self::Output {
        if let BEncodeNode::Dict(d) = self {
            &d[index]
        } else { panic!("Tried to dictionary-index a BEncodeNode that wasn't a Dict variant.") }
    }
}

impl std::ops::Index<usize> for BEncodeNode {
    type Output = BEncodeNode;
    fn index(&self, index: usize) -> &Self::Output {
        if let BEncodeNode::List(l) = self {
            &l[index]
        } else { panic!("Tried to integer-index a BEncodeNode that wasn't a List variant.") }
    }
}

impl BEncodeNode {
    pub fn get_dict(&self, index: &[u8]) -> Option<&Self> {
        if let BEncodeNode::Dict(d) = self {
            d.get(index)
        } else { None }
    }

    pub fn get_list(&self, index: usize) -> Option<&Self> {
        if let BEncodeNode::List(l) = self {
            l.get(index)
        } else { None }
    }

    pub fn parse(text: &[u8]) -> Result<BEncodeNode, BEncodeNodeParseError> {
        let (node, count) = Self::parse_node(text)?;
        if text.len() == count {
            Ok(node)
        } else {
            Err(BEncodeNodeParseError::ExcessInput)
        }
    }

    fn parse_node(text: &[u8]) -> Result<(BEncodeNode, usize), BEncodeNodeParseError> {
        if text.len() < 1 {
            return Err(BEncodeNodeParseError::Other); // Malformed input
        }
        match text[0] {
            b'i' => {
                let end = text.iter().position(|&x| x == b'e').ok_or_else(|| BEncodeNodeParseError::UnexpectedEnd)?;
                let isizestr = std::str::from_utf8(&text[1..end]).map_err(|_e| BEncodeNodeParseError::InvalidASCIIInToken)?;
                match isizestr.parse::<isize>() {
                    Ok(i) => Ok((BEncodeNode::Integer(i), end + 1)),
                    Err(_) => Err(BEncodeNodeParseError::MalformedInteger),
                }
            }
            b'0'..=b'9' => {
                // usizestrlen INCLUDES the terminating colon ':'
                // let usizestrlen = text.iter().position(|x| !(b'0'..=b'9').contains(x)).ok_or_else(|| BEncodeNodeParseError::UnexpectedEnd)?;
                let usizestrlen = text.iter().position(|&x| x == b':').ok_or_else(|| BEncodeNodeParseError::UnexpectedEnd)?;
                let usizestr = std::str::from_utf8(&text[..usizestrlen]).map_err(|_e| BEncodeNodeParseError::InvalidASCIIInToken)?;
                match usizestr.parse::<usize>() {
                    Ok(payloadlen) => {
                        if payloadlen == 0 { return Ok((BEncodeNode::ByteString(Vec::new()), 2)); }
                        if text.len() < usizestrlen + 1 + payloadlen { return Err(BEncodeNodeParseError::UnexpectedEnd); }
                        let payload = &text[usizestrlen + 1..usizestrlen + 1 + payloadlen];
                        Ok((BEncodeNode::ByteString(payload.to_vec()), usizestrlen + 1 + payloadlen))
                    }
                    Err(_) => Err(BEncodeNodeParseError::MalformedInteger),
                }
            }
            b'l' => {
                Self::parse_list(&text[1..]).map(|ret| (BEncodeNode::List(ret.0), ret.1 + 2))
            }
            b'd' => {
                Self::parse_dict(&text[1..]).map(|ret| (BEncodeNode::Dict(ret.0), ret.1 + 2))
            }
            b'e' => Err(BEncodeNodeParseError::DanglingEndMarker),
            _ => Err(BEncodeNodeParseError::Other)  //malformed input
        }
    }

    fn parse_list(mut text: &[u8]) -> Result<(Vec<BEncodeNode>, usize), BEncodeNodeParseError> {
        // For the "consumed characters" return field, this function considers ONLY the inner part of the list.
        // e.g. consumed("l8:lareputai42ee") == 14, NOT 16.
        let mut retval = Vec::new();
        let mut total_consumed = 0;
        loop {
            match Self::parse_node(text) {
                Ok((node, consumed)) => {
                    retval.push(node);
                    total_consumed += consumed;
                    text = &text[consumed..];
                }
                Err(e) if e == BEncodeNodeParseError::DanglingEndMarker => {
                    return Ok((retval, total_consumed));
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }

    fn parse_dict(mut text: &[u8]) -> Result<(HashMap<Vec<u8>, BEncodeNode>, usize), BEncodeNodeParseError> {
        // For the "consumed characters" return field, this function considers ONLY the inner part of the dict.
        // e.g. consumed("d3:bar4:spam3:fooi42ee") == 20, NOT 22.
        // d3:bar4:spam3:fooi42ee == { 'bar': 'spam', 'foo': 42 }
        let mut retval = HashMap::new();
        let mut total_consumed = 0;
        loop {
            match Self::parse_node(text) {
                Ok(r1) => {
                    // If we successfully read one inner node (which MUST be of type ByteString) then we must also be able to successfully read a second node.
                    if ! matches!(r1.0, BEncodeNode::ByteString(_)) {
                        return Err(BEncodeNodeParseError::InvalidDictKey); // Wrong type of Node object
                    }
                    text = &text[r1.1..];
                    let r2 = Self::parse_node(text)?; // If we read r1 but can't read r2, something went wrong and we just propagate it upwards.
                    text = &text[r2.1..];
                    total_consumed += r1.1 + r2.1;
                    if let (BEncodeNode::ByteString(k), v) = (r1.0, r2.0) {
                        retval.insert(k, v);
                    } else { unreachable!() } // Guarded by the if ! matches!(...) above.
                }
                Err(e) if e == BEncodeNodeParseError::DanglingEndMarker => {
                    return Ok((retval, total_consumed));
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }

    // Opposite of parse: Generate the (singular valid) BENCODEd representation of the object `self`.
    pub fn emit(&self) -> String {
        todo!()
    }

    pub fn render_bytestring(bs: &[u8]) -> String {
        let mut retval = String::with_capacity(bs.len());
        for char in bs {
            if char.is_ascii_alphanumeric() || char.is_ascii_whitespace() || char.is_ascii_punctuation() {
                retval.push(*char as char);
            }
        }
        retval
    }

    pub fn render_plain(&self) -> String {
        // Deliberatly omits binary bytestrings from the textual representation.
        let mut retval = String::new();
        match self {
            BEncodeNode::Integer(i) => retval.push_str(&i.to_string()),
            BEncodeNode::ByteString(s) => retval.push_str(&BEncodeNode::render_bytestring(s)),
            BEncodeNode::List(l) => {
                let cheese = join(
                    l.iter().map(|itm| itm.render_plain()),
                    ", "
                );
                retval.push_str(&format!("[{}]", cheese));
            }
            BEncodeNode::Dict(d) => {
                let cheese = join(
                    d.iter()
                        .sorted_by_key(|&(k, v)| k)
                        .map(|(k,v)| {
                            if k == b"pieces" {
                                format!("pieces: <OUTPUT SUPPRESSED>")
                            } else {
                                format!("{}: {}", BEncodeNode::render_bytestring(k), v.render_plain())
                            }
                        }),
                    ", "
                );
                retval.push_str(&format!("{{{}}}", cheese));
            }
            // BEncodeNode::Dict(d) => {
            //     let mut list : Vec<(&Vec<u8>, &BEncodeNode)> = d.into_iter().collect();
            //     list.sort_unstable_by_key(|itm| itm.0);
            //     let mut render1 : Vec<String> = list.into_iter()
            //         .map(|(k,v)| {
            //             match &k[..] {
            //                 b"pieces" => format!("pieces: <OUTPUT SUPPRESSED>"),
            //                 _ => format!("{}: {}", BEncodeNode::render_bytestring(k), v.render_plain())
            //             }
            //         })
            //         .collect();
            //     let cheese : String = join(render1, ", ");
            //     retval.push_str(&format!("{{{}}}", cheese));
            // }
        }
        retval
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use super::*;

    #[test]
    fn test_parse_invalid_bencode() {
        assert_eq!(BEncodeNode::parse_node(b"this is not a valid bencoded string"), Err(BEncodeNodeParseError::Other));
    }

    #[test]
    fn test_parse_integer() {
        let i1 = BEncodeNode::Integer(42);
        let i2 = BEncodeNode::Integer(-42);
        let i3 = BEncodeNode::Integer(0);

        assert_eq!(BEncodeNode::parse_node(b"i42e"), Ok((i1, 4)));
        assert_eq!(BEncodeNode::parse_node(b"i-42e"), Ok((i2, 5)));
        assert_eq!(BEncodeNode::parse_node(b"i0e"), Ok((i3, 3)));
        assert_eq!(BEncodeNode::parse_node(b"ie"), Err(BEncodeNodeParseError::MalformedInteger));
        assert_eq!(BEncodeNode::parse_node(b"i-42"), Err(BEncodeNodeParseError::UnexpectedEnd));
        assert_eq!(BEncodeNode::parse_node(b"i-\xEB42"), Err(BEncodeNodeParseError::UnexpectedEnd));
    }

    #[test]
    fn test_parse_bytestring() {
        let s1 = BEncodeNode::ByteString(b"test".to_vec());
        let s2 = BEncodeNode::ByteString(b"".to_vec());
        let s3 = BEncodeNode::ByteString(b"lareputa".to_vec());
        let s4 = BEncodeNode::ByteString(b"lareputa".to_vec());

        assert_eq!(BEncodeNode::parse_node(b"4:test"), Ok((s1, 6)));
        assert_eq!(BEncodeNode::parse_node(b"0:"), Ok((s2, 2)));
        assert_eq!(BEncodeNode::parse_node(b"8:lareputa"), Ok((s3, 10)));
        assert_eq!(BEncodeNode::parse_node(b"8:lareputaaaaaaaaaaaa"), Ok((s4, 10)));
        assert_eq!(BEncodeNode::parse_node(b"8:lare"), Err(BEncodeNodeParseError::UnexpectedEnd));
        assert_eq!(BEncodeNode::parse_node(b"84menos2:lare"), Err(BEncodeNodeParseError::MalformedInteger));
        // TODO more tests
    }

    #[test]
    fn test_parse_list() {
        let i1 = BEncodeNode::Integer(42);
        let s3 = BEncodeNode::ByteString(b"lareputa".to_vec());
        let l1 = BEncodeNode::List(vec![s3,i1]);

        assert_eq!(BEncodeNode::parse_node(b"l8:lareputai42ee"), Ok((l1, 16)));
        // TODO more tests
    }

    #[test]
    fn test_parse_dict() {
        let k1 = b"bar".to_vec();
        let s1 = BEncodeNode::ByteString(b"spam".to_vec());
        let k2 = b"foo".to_vec();
        let i1 = BEncodeNode::Integer(42);
        let d1 = BEncodeNode::Dict(HashMap::from([
            (k1.clone(), s1.clone()),
            (k2.clone(), i1.clone())
        ]));

        assert_eq!(BEncodeNode::parse_node(b"d3:bar4:spam3:fooi42ee"), Ok((d1,22)));
        assert_eq!(BEncodeNode::parse_node(b"d3:bar4:spami42e3:fooe"), Err(BEncodeNodeParseError::InvalidDictKey));
        // TODO more tests
    }

    #[test]
    fn test_parse_excess_input() {
        let k1 = b"bar".to_vec();
        let s1 = BEncodeNode::ByteString(b"spam".to_vec());
        let k2 = b"foo".to_vec();
        let i1 = BEncodeNode::Integer(42);
        let d1 = BEncodeNode::Dict(HashMap::from([
            (k1, s1),
            (k2, i1)
        ]));

        assert_eq!(BEncodeNode::parse(b"d3:bar4:spam3:fooi42eeTHISISTOOMUCHTEXT"), Err(
            BEncodeNodeParseError::ExcessInput
        ));
        assert_eq!(BEncodeNode::parse(b"d3:bar4:spam3:fooi42ee"), Ok(d1));
    }

    #[test]
    fn test_render_plain() {
        let i1 = BEncodeNode::Integer(42);
        let s1 = BEncodeNode::ByteString(b"lareputa".to_vec());

        let i2 = BEncodeNode::Integer(42);
        let s2 = BEncodeNode::ByteString(b"lareputa".to_vec());
        let l1 = BEncodeNode::List(vec![s2,i2]);

        let k1 = b"bar".to_vec();
        let v1 = BEncodeNode::ByteString(b"spam".to_vec());
        let k2 = b"foo".to_vec();
        let v2 = BEncodeNode::Integer(42);
        let d1 = BEncodeNode::Dict(HashMap::from([
            (k1, v1),
            (k2, v2)
        ]));

        assert_eq!("42", i1.render_plain());
        assert_eq!("lareputa", s1.render_plain());
        assert_eq!("[lareputa, 42]", l1.render_plain());
        assert_eq!("{bar: spam, foo: 42}", d1.render_plain());

    }
}
