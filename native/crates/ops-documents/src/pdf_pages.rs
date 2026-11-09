//! Bounded page-tree traversal and stream decompression; repeated content is charged again as work.
use crate::{
    pdf_container::{Document, MAX_STREAM, ObjectId, decode, get},
    pdf_limits::{MAX_CONTENT_DECODED, deadline},
    pdf_syntax::{Dict, Value, bad, limit},
};
use ops_protocol::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};
/// Pages one walk returns: the physical bound a page offset cannot move.
pub const MAX_PAGES: usize = 32;
/// One page's dictionary and the /Resources in effect for it.
pub struct Page<'a> {
    pub dict: &'a Dict,
    pub resources: Option<&'a Value>,
}
/// One bounded page-tree walk: the pages it found and whether the physical bound stopped it.
pub struct PageTree<'a> {
    /// At most MAX_PAGES pages, in page-tree order.
    pub pages: Vec<Page<'a>>,
    /// A page beyond MAX_PAGES exists, so pages holds exactly MAX_PAGES entries.
    pub truncated: bool,
}
fn reference(value: &Value) -> Result<ObjectId> {
    match value {
        Value::Ref(n, g) => Ok((*n, *g)),
        _ => Err(bad("PDF page/content identity must be indirect")),
    }
}
/// Resolve the page-tree /Count as a bounded nonnegative integer; an indirect /Count is legal, so
/// its reference chain keeps its own failure instead of a misleading type error.
fn page_count(doc: &Document<'_>, dict: &Dict) -> Result<usize> {
    let value = doc.resolve(get(dict, b"Count")?).map_err(|mut failure| {
        failure.message = format!(
            "PDF page tree Count reference cannot be used: {}",
            failure.message
        );
        failure
    })?;
    value.integer().map_err(|_| {
        bad("PDF page tree Count must be a bounded nonnegative integer or an indirect reference to one")
    })
}
/// Collect at most MAX_PAGES pages in page-tree order; a page beyond the bound sets truncated
/// instead of failing, and /Count is verified only after a completed walk.
pub fn pages<'a>(doc: &'a Document<'_>, started: Instant) -> Result<PageTree<'a>> {
    let catalog = doc.object(doc.root)?.value.dict()?;
    if get(catalog, b"Type")?.name()? != b"Catalog" {
        return Err(bad("PDF Root must be a Catalog"));
    }
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    let mut truncated = false;
    walk(
        doc,
        reference(get(catalog, b"Pages")?)?,
        None,
        None,
        &mut seen,
        &mut output,
        &mut truncated,
        started,
        0,
    )?;
    Ok(PageTree {
        pages: output,
        truncated,
    })
}
#[allow(clippy::too_many_arguments)]
fn walk<'a>(
    doc: &'a Document<'_>,
    id: ObjectId,
    parent: Option<ObjectId>,
    inherited: Option<&'a Value>,
    seen: &mut BTreeSet<ObjectId>,
    output: &mut Vec<Page<'a>>,
    truncated: &mut bool,
    started: Instant,
    depth: usize,
) -> Result<usize> {
    deadline(started)?;
    if depth > 64 || seen.len() >= 10_000 {
        return Err(limit("PDF page tree exceeds bounded depth or nodes"));
    }
    if !seen.insert(id) {
        return Err(bad("Cyclic or aliased PDF page tree"));
    }
    let object = doc.object(id)?;
    if object.stream.is_some() {
        return Err(bad("PDF page tree node cannot be a stream"));
    }
    let dict = object.value.dict()?;
    if let Some(parent) = parent {
        if reference(get(dict, b"Parent")?)? != parent {
            return Err(bad("PDF page Parent disagrees with page tree"));
        }
    } else if dict.contains_key(b"Parent".as_slice()) {
        return Err(bad("PDF page-tree root cannot have a Parent"));
    }
    let resources = dict.get(b"Resources".as_slice()).or(inherited);
    match get(dict, b"Type")?.name()? {
        b"Page" => {
            if output.len() >= MAX_PAGES {
                *truncated = true;
                return Ok(0);
            }
            output.push(Page { dict, resources });
            Ok(1)
        }
        b"Pages" => {
            let expected = page_count(doc, dict)?;
            let Value::Array(kids) = get(dict, b"Kids")? else {
                return Err(bad("PDF Pages Kids must be an array"));
            };
            let mut count = 0;
            for kid in kids {
                // The first page beyond the bound proves truncation; nothing after it is read.
                if *truncated {
                    break;
                }
                count += walk(
                    doc,
                    reference(kid)?,
                    Some(id),
                    resources,
                    seen,
                    output,
                    truncated,
                    started,
                    depth + 1,
                )?;
            }
            if !*truncated && count != expected {
                return Err(bad("PDF page Count disagrees with observed tree"));
            }
            Ok(count)
        }
        _ => Err(bad("PDF page tree node has an invalid Type")),
    }
}
/// The page content streams decoded for one file, cached by object: decoded charges each distinct
/// stream once, processed charges every page that names it.
pub struct Streams {
    cache: BTreeMap<ObjectId, Vec<u8>>,
    pub decoded: usize,
    pub processed: usize,
}
impl Streams {
    /// An empty cache with both byte counters at zero.
    pub fn new() -> Self {
        Self {
            cache: BTreeMap::new(),
            decoded: 0,
            processed: 0,
        }
    }
    fn append(
        &mut self,
        doc: &Document<'_>,
        value: &Value,
        output: &mut Vec<u8>,
        started: Instant,
    ) -> Result<()> {
        deadline(started)?;
        let id = reference(value)?;
        if !self.cache.contains_key(&id) {
            let bytes = decode(doc.object(id)?)?;
            deadline(started)?;
            if self.decoded + bytes.len() > MAX_CONTENT_DECODED {
                return Err(limit("PDF aggregate decoded streams exceed 16 MiB"));
            }
            self.decoded += bytes.len();
            self.cache.insert(id, bytes);
        }
        let bytes = &self.cache[&id];
        if self.processed + bytes.len() > MAX_CONTENT_DECODED || output.len() + bytes.len() > MAX_STREAM {
            return Err(limit(
                "PDF processed content exceeds 16 MiB total or 1 MiB per page",
            ));
        }
        self.processed += bytes.len();
        output.extend_from_slice(bytes);
        Ok(())
    }
    /// The decoded /Contents streams of page, concatenated in stream order; a non-stream entry is a
    /// parse error and more than 64 streams or the decode budget is OUTPUT_LIMIT.
    pub fn content(
        &mut self,
        doc: &Document<'_>,
        page: &Page<'_>,
        started: Instant,
    ) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        let Some(value) = page.dict.get(b"Contents".as_slice()) else {
            return Ok(output);
        };
        match value {
            Value::Array(values) => {
                if values.len() > 64 {
                    return Err(limit("PDF page exceeds 64 content streams"));
                }
                for value in values {
                    self.append(doc, value, &mut output, started)?
                }
            }
            Value::Null => {}
            value => self.append(doc, value, &mut output, started)?,
        }
        Ok(output)
    }
}
