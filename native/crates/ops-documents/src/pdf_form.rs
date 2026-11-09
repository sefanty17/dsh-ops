//! Form XObject traversal: form content is read where `Do` appears, within depth and decode bounds.
use crate::{
    pdf_container::{Document, Object, ObjectId, decode, get},
    pdf_limits::MAX_CONTENT_DECODED,
    pdf_syntax::{Value, bad, limit},
    pdf_text::{Fonts, traverse},
};
use ops_protocol::Result;
use std::{
    collections::BTreeSet,
    time::Instant,
};
/// Decoded form content one page may read, on top of what its own content streams decode.
const MAX_FORM_DECODED: usize = 1024 * 1024;
/// Form XObjects one page may nest inside each other before OUTPUT_LIMIT.
const MAX_FORM_DEPTH: usize = 8;
/// The value of the object one page /XObject resource names; a stream object cannot be resolved as
/// a plain value, so its value is read directly under the bounded reference chain.
fn xobject<'a>(doc: &'a Document<'_>, value: &'a Value) -> Result<&'a Value> {
    let mut current = value;
    let mut seen = BTreeSet::new();
    while let Value::Ref(number, generation) = current {
        if seen.len() >= 64 {
            return Err(limit("PDF reference chain exceeds 64"));
        }
        if !seen.insert((*number, *generation)) {
            return Err(bad("Cyclic PDF reference"));
        }
        current = &doc.object((*number, *generation))?.value;
    }
    Ok(current)
}
/// The object a form /XObject resource names: only an indirect stream carries the form's content,
/// and the chain keeps its 64-hop bound and cycle check.
fn form_stream<'data, 'doc>(
    doc: &'doc Document<'data>,
    value: &'doc Value,
) -> Result<(ObjectId, &'doc Object<'data>)> {
    let mut current = value;
    let mut seen = BTreeSet::new();
    loop {
        let Value::Ref(number, generation) = current else {
            return Err(bad("PDF form XObject resource does not name a stream"));
        };
        let id = (*number, *generation);
        if seen.len() >= 64 {
            return Err(limit("PDF reference chain exceeds 64"));
        }
        if !seen.insert(id) {
            return Err(bad("Cyclic PDF reference"));
        }
        let object = doc.object(id)?;
        if object.stream.is_some() {
            return Ok((id, object));
        }
        current = &object.value;
    }
}
/// Traverse an XObject named by `Do`: images paint no glyph and return no runs. A form contributes
/// its text runs in content order, decoded bytes charged to the document's shared decoded budget
/// and `form_bytes` bounding the form content one page reads.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint(
    doc: &Document<'_>,
    resources: Option<&Value>,
    name: &[u8],
    started: Instant,
    fonts: &mut Fonts,
    decoded: &mut usize,
    form_bytes: &mut usize,
    expanding: &mut BTreeSet<ObjectId>,
    depth: usize,
) -> Result<Vec<String>> {
    let page_resources = doc
        .resolve(resources.ok_or_else(|| bad("Missing PDF page Resources"))?)?
        .dict()?;
    let xobjects = doc.resolve(get(page_resources, b"XObject")?)?.dict()?;
    let resource = get(xobjects, name)?;
    let target = xobject(doc, resource)?;
    let target_dict = target.dict()?;
    match target_dict.get(b"Subtype".as_slice()) {
        Some(Value::Name(subtype)) if subtype.as_slice() == b"Image" => Ok(Vec::new()),
        Some(Value::Name(subtype)) if subtype.as_slice() == b"Form" => {
            if depth >= MAX_FORM_DEPTH {
                return Err(limit("PDF form XObjects nest more than 8 levels"));
            }
            let (id, object) = form_stream(doc, resource)?;
            if !expanding.insert(id) {
                return Err(bad("Cyclic PDF form XObject reference"));
            }
            // The id stays in expanding during traversal and is removed when it returns, so a
            // sibling may reference it again; a decode or budget failure returns before removal.
            let form = decode(object)?;
            *form_bytes = (*form_bytes).saturating_add(form.len());
            if *form_bytes > MAX_FORM_DECODED {
                return Err(limit(
                    "PDF form XObject decoded bytes exceed 1 MiB per page",
                ));
            }
            if *decoded + form.len() > MAX_CONTENT_DECODED {
                return Err(limit("PDF aggregate decoded streams exceed 16 MiB"));
            }
            *decoded += form.len();
            let inherited = target_dict.get(b"Resources".as_slice()).or(resources);
            let nested = traverse(
                doc,
                inherited,
                &form,
                started,
                fonts,
                decoded,
                form_bytes,
                expanding,
                depth + 1,
            );
            expanding.remove(&id);
            nested
        }
        Some(Value::Name(subtype)) => Err(crate::pdf_syntax::unsupported(format!(
            "Unsupported PDF content: XObject /{} has /Subtype /{}; only image and form \
             XObjects are traversed",
            String::from_utf8_lossy(name),
            String::from_utf8_lossy(subtype.as_slice())
        ))),
        Some(_) => Err(bad(format!(
            "PDF XObject /{} has a /Subtype that is not a name",
            String::from_utf8_lossy(name)
        ))),
        None => Err(bad(format!(
            "PDF XObject /{} has no /Subtype",
            String::from_utf8_lossy(name)
        ))),
    }
}
