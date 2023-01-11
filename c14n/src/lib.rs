//! This crate is part of [Sophia],
//! an [RDF] and [Linked Data] toolkit in Rust.
//!
//! This crate provides function to canonicalize graphs and datasets,
//! following the algorithm described at:
//!
//!   https://www.w3.org/TR/rdf-canon/
//!
//! For the moment, the algorithms in this module *panic*
//! when faced with RDF-star or variables.
//!
//! TODO list:
//! - [ ] make the code more modular
//! - [ ] remove all 'panic's and raise an approriate error instead
//! - [ ] provide a way to customize the max recursion depth
//! - [ ] check that UTF-8 byte-by-byte ordering is equivalent to code point ordering,
//!       and if not, fix all the places with a 'FIX? code point ordering' comment
//! - [ ] implement test suite
use std::collections::btree_map::Entry::*;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::rc::Rc;

use sophia_api::dataset::{DTerm, Dataset};
use sophia_api::quad::{iter_spog, Quad, Spog};
use sophia_api::term::{BnodeId, Term, TermKind};

mod _c14n_term;
use _c14n_term::C14nTerm;
mod _permutations;
use _permutations::for_each_permutation_of;

/// Return the sorted n-quad using canonical blank node labels for `d`.
///
pub fn c14n_nquads<D: Dataset>(d: &D) -> Result<String, D::Error> {
    let mut lines: Vec<_> = c14n_dataset(d)?
        .iter()
        .map(|q| {
            let mut line = String::new();
            nq(q.s(), &mut line);
            nq(q.p(), &mut line);
            nq(q.o(), &mut line);
            if let Some(gn) = q.g() {
                nq(gn, &mut line);
            }
            line.push_str(".\n");
            line
        })
        .collect();
    lines.sort_unstable(); // FIX? code point order ?
    let mut output = String::new();
    for line in lines {
        output.push_str(&line);
    }
    Ok(output)
}

/// Return a canonical form of the dataset `d`.
///
/// Implements https://www.w3.org/TR/rdf-canon/#canon-algorithm
pub fn c14n_dataset<'a, D: Dataset>(d: &'a D) -> Result<C14nQuads<'a, D>, D::Error> {
    let quads: Result<Vec<Spog<DTerm<'a, D>>>, _> =
        d.quads().map(|res| res.map(Quad::to_spog)).collect();
    let quads = quads?;
    // Step 1
    let mut state = C14nState::new();
    // Step 2
    for quad in &quads {
        for component in iter_spog(quad.spog()) {
            assert!(!component.is_triple() && !component.is_variable());
            if let Some(bnid) = component.bnode_id() {
                match state.b2q.entry(Rc::from(bnid.as_str())) {
                    Vacant(e) => {
                        e.insert(vec![quad]);
                    }
                    Occupied(mut e) => e.get_mut().push(quad),
                }
            }
        }
    }
    // Step 3
    for (bnid, quads) in state.b2q.iter() {
        let hash = hash_first_degree_quads(bnid, &quads[..]);
        let bnid2 = Rc::clone(bnid);
        match state.h2b.entry(hash) {
            Vacant(e) => {
                e.insert(vec![bnid2]);
            }
            Occupied(mut e) => e.get_mut().push(bnid2),
        }
        state.b2h.insert(Rc::clone(bnid), hash);
    }
    // Step 4
    // NB: we are relying on the fact that BTreeMap's elements are sorted
    let mut next_h2b = BTreeMap::new();
    // TODO once BTreeMap::drain_filter is stabilize,
    // use it in the loop below instead of reinserting elements into a new map
    for (hash, bnids) in state.h2b.into_iter() {
        debug_assert!(!bnids.is_empty());
        if bnids.len() > 1 {
            next_h2b.insert(hash, bnids);
        } else {
            state.canonical.issue(&bnids[0]);
        }
    }
    state.h2b = next_h2b;
    // Step 5
    for identifier_list in state.h2b.values() {
        let mut hash_path_list = vec![];
        // Step 5.2
        for n in identifier_list {
            let mut issuer = BnodeIssuer::new(BnodeId::new_unchecked("b"));
            issuer.issue(n);
            hash_path_list.push(state.hash_n_degree_quads(n, &issuer, 0));
        }
        // Step 5.3
        hash_path_list.sort_unstable_by_key(|p| p.0);
        for (_, issuer) in hash_path_list {
            for bnid in issuer.issued_order {
                state.canonical.issue(&bnid);
            }
        }
    }
    // Step 6
    let issued = state.canonical.issued;
    Ok(quads
        .into_iter()
        .map(|q| {
            let (spo, g) = q;
            let convert = |t: DTerm<'a, D>| {
                if let Some(bnid) = t.bnode_id() {
                    let canon_id = issued.get(bnid.as_str()).unwrap();
                    return C14nTerm::Blank(canon_id.clone());
                }
                C14nTerm::Other(t)
            };
            let spo = spo.map(convert);
            let g = g.map(convert);
            (spo, g)
        })
        .collect())
}

type C14nQuads<'a, D> = Vec<Spog<C14nTerm<DTerm<'a, D>>>>;

#[derive(Clone, Debug)]
struct C14nState<'a, T: Term> {
    b2q: BTreeMap<Rc<str>, Vec<&'a Spog<T>>>,
    h2b: BTreeMap<Hash, Vec<Rc<str>>>,
    canonical: BnodeIssuer,
    /// Not specified in the spec: memozing the results of hash 1st degree
    b2h: BTreeMap<Rc<str>, Hash>,
    /// Not specified in the spec: maximum recursion in hash_n_degree_quads
    max_depth: usize,
}

impl<'a, T: Term> C14nState<'a, T> {
    fn new() -> Self {
        C14nState {
            b2q: BTreeMap::new(),
            h2b: BTreeMap::new(),
            canonical: BnodeIssuer::new(BnodeId::new_unchecked("c14n")),
            b2h: BTreeMap::new(),
            max_depth: 16,
        }
    }

    /// Implements https://www.w3.org/TR/rdf-canon/#hash-related-blank-node
    fn hash_related_bnode(
        &self,
        related: &str,
        quad: &Spog<T>,
        issuer: &BnodeIssuer,
        position: &str,
    ) -> Hash {
        let mut input = hmac_sha256::Hash::new();
        input.update(position.as_bytes());
        if position != "g" {
            input.update(b"<");
            input.update(quad.p().iri().unwrap().as_bytes());
            input.update(b">");
        }
        if let Some(canon_id) = self.canonical.issued.get(related) {
            input.update(b"_:");
            input.update(canon_id.as_bytes());
        } else if let Some(temp_id) = issuer.issued.get(related) {
            input.update("_:");
            input.update(temp_id.as_bytes());
        } else {
            // retrieved memoized value of hash_first_degree_quads for this blank node
            let h1d = self.b2h.get(related).unwrap();
            input.update(hex(h1d).as_bytes());
        }
        input.finalize()
    }

    /// Implements https://www.w3.org/TR/rdf-canon/#hash-nd-quads
    fn hash_n_degree_quads(
        &self,
        identifier: &str,
        issuer: &BnodeIssuer,
        depth: usize,
    ) -> (Hash, BnodeIssuer) {
        if depth > self.max_depth {
            panic!("too many recursions");
        }
        // Step 1
        let mut hn = BTreeMap::<Hash, Vec<Box<str>>>::new();
        // Step 2
        let quads = self.b2q.get(identifier).unwrap();
        // Step 3
        for quad in quads {
            for (component, position) in iter_spog(quad.spog()).zip(["s", "p", "o", "g"].iter()) {
                assert!(!component.is_triple() && !component.is_variable());
                if let Some(bnid) = component.bnode_id() {
                    if &bnid == identifier {
                        continue;
                    }
                    let hash = self.hash_related_bnode(&bnid, quad, issuer, position);
                    let bnid = Box::from(bnid.as_str());
                    match hn.entry(hash) {
                        Vacant(e) => {
                            e.insert(vec![bnid]);
                        }
                        Occupied(mut e) => e.get_mut().push(bnid),
                    }
                }
            }
        }
        // Step 4
        let mut data_to_hash = hmac_sha256::Hash::new();
        // Step 5
        let mut ret_issuer: Option<BnodeIssuer> = None;
        for (related_hash, mut blank_node) in hn.into_iter() {
            data_to_hash.update(&hex(&related_hash));
            let mut chosen_path = String::new();
            let mut chosen_issuer: Option<BnodeIssuer> = None;
            // Step 5.4
            for_each_permutation_of(&mut blank_node, |p| {
                let mut issuer_copy = ret_issuer.as_ref().unwrap_or(issuer).clone();
                let mut path = String::new();
                let mut recursion_list = vec![];
                // Step 5.4.4
                for related in p {
                    if let Some(canon_id) = self.canonical.issued.get(related.as_ref()) {
                        path.push_str("_:");
                        path.push_str(canon_id);
                    } else {
                        let (id, new) = issuer_copy.issue(related);
                        if new {
                            recursion_list.push(related.as_ref());
                        }
                        path.push_str("_:");
                        path.push_str(id);
                    }
                }
                if !chosen_path.is_empty() && smaller_path(&chosen_path, &path) {
                    return; // skip to the next permutation
                }
                // Step 5.4.5
                for related in recursion_list {
                    let result = self.hash_n_degree_quads(related, &issuer_copy, depth + 1);
                    let (id, _) = issuer_copy.issue(related);
                    path.push_str("_:");
                    path.push_str(id);
                    path.push('<');
                    path.push_str(&hex(&result.0));
                    path.push('>');
                    issuer_copy = result.1;
                    if !chosen_path.is_empty() && smaller_path(&chosen_path, &path) {
                        return; // skip to the next permutation
                    }
                }
                // Step 5.4.6
                if chosen_path.is_empty() || path < chosen_path {
                    // FIX? code point order ?
                    chosen_path = path;
                    chosen_issuer = Some(issuer_copy);
                }
            });
            data_to_hash.update(chosen_path.as_bytes());
            ret_issuer = chosen_issuer;
        }
        let ret = (data_to_hash.finalize(), ret_issuer.unwrap());
        debug_assert!({
            println!(
                "hash-n-degree({}, {})\n-> {}",
                identifier,
                depth,
                hex(&ret.0)
            );
            true
        });
        ret
    }
}

#[derive(Clone, Debug)]
struct BnodeIssuer {
    prefix: BnodeId<&'static str>,
    //counter: usize, // use issued_order.len() instead
    issued: BTreeMap<Rc<str>, BnodeId<Rc<str>>>,
    // Not specified in the spec: allows to keep the order in which identifiers were issued
    issued_order: Vec<Rc<str>>,
}

impl BnodeIssuer {
    fn new(prefix: BnodeId<&'static str>) -> Self {
        BnodeIssuer {
            prefix,
            issued: BTreeMap::new(),
            issued_order: vec![],
        }
    }

    /// Implements https://www.w3.org/TR/rdf-canon/#issue-identifier
    /// modified to also return a boolean indicating whether the issued identifier
    /// was newly created (true) or if it existed before (false)
    fn issue(&mut self, bnid: &str) -> (&str, bool) {
        let key = Rc::from(bnid);
        let key2 = Rc::clone(&key);
        match self.issued.entry(key) {
            Occupied(e) => (e.into_mut().as_str(), false),
            Vacant(e) => {
                let counter = self.issued_order.len();
                let ret = e.insert(BnodeId::new_unchecked(
                    format!("{}{}", self.prefix.as_str(), counter).into(),
                ));
                self.issued_order.push(key2);
                (ret.as_str(), true)
            }
        }
    }
}

type Hash = [u8; 32];

/// Implements https://www.w3.org/TR/rdf-canon/#hash-1d-quads
/// with the difference that the C14n state is not passed;
/// instead, the quad list corresponding to bnid is passed directly
fn hash_first_degree_quads<Q: Quad>(bnid: &str, quads: &[&Q]) -> Hash {
    let mut nquads: Vec<_> = quads
        .iter()
        .map(|q| {
            let mut line = String::new();
            nq_for_hash(q.s(), &mut line, bnid);
            nq_for_hash(q.p(), &mut line, bnid);
            nq_for_hash(q.o(), &mut line, bnid);
            if let Some(gn) = q.g() {
                nq_for_hash(gn, &mut line, bnid);
            }
            line.push_str(".\n");
            line
        })
        .collect();
    nquads.sort_unstable(); // FIX? code point order ?
    let mut hasher = hmac_sha256::Hash::new();
    for line in nquads.into_iter() {
        hasher.update(&line);
    }
    let ret = hasher.finalize();
    debug_assert!({
        println!("hash-fisrt-degree({})\n-> {}", bnid, hex(&ret));
        true
    });
    ret
}

fn nq_for_hash<T: Term>(term: T, buffer: &mut String, ref_bnid: &str) {
    if let Some(bnid) = term.bnode_id() {
        if &bnid == ref_bnid {
            buffer.push_str("_:a ");
        } else {
            buffer.push_str("_:z ");
        }
    } else {
        nq(term.borrow_term(), buffer);
    }
}

fn nq<T: Term>(term: T, buffer: &mut String) {
    match term.kind() {
        TermKind::Iri => {
            buffer.push('<');
            buffer.push_str(&term.iri().unwrap());
            buffer.push('>');
        }
        TermKind::Literal => {
            buffer.push('"');
            for c in term.lexical_form().unwrap().chars() {
                match c {
                    '"' => buffer.push_str("\\\""),
                    '\\' => buffer.push_str("\\\\"),
                    '\n' => buffer.push_str("\\n"),
                    '\r' => buffer.push_str("\\r"),
                    _ => buffer.push(c),
                }
            }
            buffer.push('"');
            if let Some(tag) = term.language_tag() {
                buffer.push('@');
                buffer.push_str(&tag);
            } else {
                buffer.push_str("^^");
                nq(term.datatype().unwrap(), buffer);
            }
        }
        TermKind::BlankNode => {
            buffer.push_str("_:");
            buffer.push_str(&term.bnode_id().unwrap());
        }
        TermKind::Triple => panic!("quoted triples not supported by c14n"),
        TermKind::Variable => panic!("variables not supported by c14n"),
    }
    buffer.push(' ');
}

fn hex(hash: &Hash) -> String {
    let mut digest = String::with_capacity(64);
    for b in hash {
        write!(&mut digest, "{:02x}", b).unwrap();
    }
    digest
}

fn smaller_path(path1: &str, path2: &str) -> bool {
    use std::cmp::Ordering::*;
    match path1.len().cmp(&path2.len()) {
        Less => true,
        Equal => path1 < path2,
        Greater => false,
    }
}

#[cfg(test)]
mod test {
    use sophia_api::term::{LanguageTag, SimpleTerm, VarName};

    use super::*;

    #[test]
    fn example2() {
        let dataset = ez_quads(&[
            "<http://example.com/#p> <http://example.com/#q> _:e0 .",
            "<http://example.com/#p> <http://example.com/#r> _:e1 .",
            "_:e0 <http://example.com/#s> <http://example.com/#u> .",
            "_:e1 <http://example.com/#t> <http://example.com/#u> .",
        ]);
        let exp = r#"<http://example.com/#p> <http://example.com/#q> _:c14n0 .
<http://example.com/#p> <http://example.com/#r> _:c14n1 .
_:c14n0 <http://example.com/#s> <http://example.com/#u> .
_:c14n1 <http://example.com/#t> <http://example.com/#u> .
"#;
        let got = c14n_nquads(&dataset).unwrap();
        println!(">>>> GOT\n{}>>>> EXPECTED\n{}<<<<", got, exp);
        assert!(got == exp);
    }

    #[test]
    fn example3() {
        let dataset = ez_quads(&[
            "<http://example.com/#p> <http://example.com/#q> _:e0 .",
            "<http://example.com/#p> <http://example.com/#q> _:e1 .",
            "_:e0 <http://example.com/#p> _:e2 .",
            "_:e1 <http://example.com/#p> _:e3 .",
            "_:e2 <http://example.com/#r> _:e3 .",
        ]);
        let exp = r#"<http://example.com/#p> <http://example.com/#q> _:c14n2 .
<http://example.com/#p> <http://example.com/#q> _:c14n3 .
_:c14n0 <http://example.com/#r> _:c14n1 .
_:c14n2 <http://example.com/#p> _:c14n1 .
_:c14n3 <http://example.com/#p> _:c14n0 .
"#;
        let got = c14n_nquads(&dataset).unwrap();
        println!(">>>> GOT\n{}>>>> EXPECTED\n{}<<<<", got, exp);
        assert!(got == exp);
    }

    #[test]
    fn cycle5() {
        let dataset = ez_quads(&[
            "_:e0 <http://example.com/#p> _:e1 .",
            "_:e1 <http://example.com/#p> _:e2 .",
            "_:e2 <http://example.com/#p> _:e3 .",
            "_:e3 <http://example.com/#p> _:e4 .",
            "_:e4 <http://example.com/#p> _:e0 .",
        ]);
        let exp = r#"_:c14n0 <http://example.com/#p> _:c14n4 .
_:c14n1 <http://example.com/#p> _:c14n0 .
_:c14n2 <http://example.com/#p> _:c14n1 .
_:c14n3 <http://example.com/#p> _:c14n2 .
_:c14n4 <http://example.com/#p> _:c14n3 .
"#;
        let got = c14n_nquads(&dataset).unwrap();
        println!(">>>> GOT\n{}>>>> EXPECTED\n{}<<<<", got, exp);
        assert!(got == exp);
    }

    // TODO: check this test:
    // it gives gives the same result as Ruby:RDF, but not as PyLD... */
    #[test]
    fn cycle2plus3() {
        let dataset = ez_quads(&[
            "_:e0 <http://example.com/#p> _:e1 .",
            "_:e1 <http://example.com/#p> _:e0 .",
            "_:e2 <http://example.com/#p> _:e3 .",
            "_:e3 <http://example.com/#p> _:e4 .",
            "_:e4 <http://example.com/#p> _:e2 .",
        ]);
        let exp = r#"_:c14n0 <http://example.com/#p> _:c14n1 .
_:c14n1 <http://example.com/#p> _:c14n0 .
_:c14n2 <http://example.com/#p> _:c14n4 .
_:c14n3 <http://example.com/#p> _:c14n2 .
_:c14n4 <http://example.com/#p> _:c14n3 .
"#;
        let got = c14n_nquads(&dataset).unwrap();
        println!(">>>> GOT\n{}>>>> EXPECTED\n{}<<<<", got, exp);
        assert!(got == exp);
    }

    /// Simplisitic Quad parser, useful for writing test cases.
    /// It is based on eq_quad below.
    fn ez_quads<'a>(lines: &[&'a str]) -> Vec<Spog<SimpleTerm<'a>>> {
        lines.iter().map(|line| ez_quad(line)).collect()
    }

    /// Simplisitic Quad parser, useful for writing test cases.
    /// The syntax is a subset of N-Quads-star,
    /// where spaces are not allowed in literals, and a space is required before the ending '.'.
    fn ez_quad(txt: &str) -> Spog<SimpleTerm> {
        let mut tokens: Vec<_> = txt.split(' ').collect();
        assert!(tokens.len() == 4 || tokens.len() == 5);
        assert!(tokens.pop().unwrap() == ".");
        let g = if tokens.len() == 4 {
            tokens.pop().map(ez_term)
        } else {
            None
        };
        let o = ez_term(tokens.pop().unwrap());
        let p = ez_term(tokens.pop().unwrap());
        let s = ez_term(tokens.pop().unwrap());
        ([s, p, o], g)
    }

    /// Simplistic Term parser, useful for writing test cases.
    /// The syntax is a subset of Turtle-star.
    fn ez_term(txt: &str) -> SimpleTerm {
        use sophia_iri::IriRef;
        match txt.as_bytes() {
            [b'<', b'<', .., b'>', b'>'] => {
                let subterms: Vec<&str> = txt[2..txt.len() - 2].split(' ').collect();
                assert_eq!(subterms.len(), 3);
                SimpleTerm::Triple(Box::new([
                    ez_term(subterms[0]),
                    ez_term(subterms[1]),
                    ez_term(subterms[2]),
                ]))
            }
            [b'<', .., b'>'] => IriRef::new_unchecked(&txt[1..txt.len() - 1]).into_term(),
            [b':', ..] => {
                let iri = format!("tag:{}", &txt[1..]);
                SimpleTerm::Iri(IriRef::new_unchecked(iri.into()))
            }
            [b'_', b':', ..] => BnodeId::new_unchecked(&txt[2..]).into_term(),
            [b'\'', .., b'\''] => (&txt[1..txt.len() - 1]).into_term(),
            [b'\'', .., b'\'', b'@', _, _] => SimpleTerm::LiteralLanguage(
                (&txt[1..txt.len() - 4]).into(),
                LanguageTag::new_unchecked(txt[txt.len() - 2..].into()),
            ),
            [c, ..] if c.is_ascii_digit() => txt.parse::<i32>().unwrap().into_term(),
            [b'?', ..] => VarName::new_unchecked(&txt[1..]).into_term(),
            _ => panic!("Unable to parse term"),
        }
    }
}
