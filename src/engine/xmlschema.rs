//! `CREATE | ALTER | DROP XML SCHEMA COLLECTION`, and the validation of typed `xml` columns (see xsd.rs).
//!
//! A collection keeps the text of its schemas; it is compiled when it is used (and the compiled form is cached per thread).
//! A column declared `xml(DOCUMENT dbo.sc)` or `xml(CONTENT dbo.sc)` is checked on every `INSERT` and `UPDATE`. Not done:
//! typed `xml` variables and parameters (they accept any xml), the typed values that XQuery would see (`.value()` on a typed
//! column reads text, as it does on an untyped one), and `ALTER XML SCHEMA COLLECTION` with anything but `ADD`.
//! Like the full-text catalogs, collections are not part of a transaction.
use super::*;
use crate::ast::XmlSchemaStmt;
use super::query::Env;
use crate::catalog::XmlSchemaCollection;
use crate::persist::LogOp;
use std::collections::HashMap;
use std::rc::Rc;

thread_local! {
    static COMPILED: std::cell::RefCell<HashMap<u64, Rc<crate::xsd::Collection>>> = std::cell::RefCell::new(HashMap::new());
}

/// The compiled form of the schemas of a collection.
pub fn compiled(texts: &[String]) -> std::result::Result<Rc<crate::xsd::Collection>, String> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    texts.hash(&mut h);
    let key = h.finish();
    if let Some(c) = COMPILED.with(|m| m.borrow().get(&key).cloned()) {
        return Ok(c);
    }
    let c = Rc::new(crate::xsd::Collection::compile(texts)?);
    COMPILED.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() > 64 {
            m.clear();
        }
        m.insert(key, c.clone());
    });
    Ok(c)
}

/// The schema documents of a text that may hold several of them side by side.
fn split_schemas(text: &str) -> std::result::Result<Vec<String>, String> {
    let doc = crate::xmlpath::Doc::parse(text).ok_or("the schema is not well-formed XML")?;
    let parts = doc.nodes("/*", &[])?;
    if parts.is_empty() {
        return Err("the schema is empty".into());
    }
    Ok(parts)
}

impl<'a> Engine<'a> {
    fn save_xml_schemas(&mut self, dbk: &str) {
        let schemas = self.srv.databases.get(dbk).map(|d| d.xml_schemas.clone()).unwrap_or_default();
        self.log_nowait(vec![LogOp::DbXmlSchemas { db: dbk.to_string(), schemas }]);
    }

    fn schema_text(&mut self, e: &Expr) -> Result<String> {
        let v = self.ev(e, &Env::empty())?;
        if v.is_null() {
            return Err(SqlError::new(6905, "XML Schema: the schema text is NULL."));
        }
        Ok(convert::to_str(&v))
    }

    pub fn exec_xml_schema(&mut self, st: &XmlSchemaStmt) -> Result<()> {
        match st {
            XmlSchemaStmt::Create { name, text } => {
                let (db, schema, n) = self.ddl_names(name);
                self.check_schema(&db, &schema)?;
                self.check_create("CREATE XML SCHEMA COLLECTION", &schema)?;
                let key = obj_key(&schema, &n);
                if self.srv.databases[&db].xml_schemas.iter().any(|x| obj_key(&x.schema, &x.name) == key) {
                    return Err(SqlError::new(6904, format!("There is already an XML schema collection named '{}' in the database.", n)));
                }
                let t = self.schema_text(text)?;
                let texts = split_schemas(&t).map_err(|m| SqlError::new(6905, format!("XML Schema: {}.", m)))?;
                compiled(&texts).map_err(|m| SqlError::new(6905, format!("XML Schema: {}.", m)))?;
                let d = self.srv.cat_mut().databases.get_mut(&db).unwrap();
                let id = 65_536 + d.xml_schemas.iter().map(|x| x.id - 65_536 + 1).max().unwrap_or(1);
                d.xml_schemas.push(XmlSchemaCollection { id, schema, name: n, texts, created: chrono::Local::now().naive_local() });
                self.save_xml_schemas(&db);
            }
            XmlSchemaStmt::Alter { name, text } => {
                let (db, schema, n) = self.ddl_names(name);
                let key = obj_key(&schema, &n);
                let Some(i) = self.srv.databases[&db].xml_schemas.iter().position(|x| obj_key(&x.schema, &x.name) == key) else {
                    return Err(SqlError::new(6903, format!("XML schema collection '{}' does not exist.", n)));
                };
                let t = self.schema_text(text)?;
                let mut texts = self.srv.databases[&db].xml_schemas[i].texts.clone();
                texts.extend(split_schemas(&t).map_err(|m| SqlError::new(6905, format!("XML Schema: {}.", m)))?);
                compiled(&texts).map_err(|m| SqlError::new(6905, format!("XML Schema: {}.", m)))?;
                self.srv.cat_mut().databases.get_mut(&db).unwrap().xml_schemas[i].texts = texts;
                self.save_xml_schemas(&db);
            }
            XmlSchemaStmt::Drop { name, if_exists } => {
                let (db, schema, n) = self.ddl_names(name);
                let key = obj_key(&schema, &n);
                let Some(i) = self.srv.databases[&db].xml_schemas.iter().position(|x| obj_key(&x.schema, &x.name) == key) else {
                    if *if_exists {
                        return Ok(());
                    }
                    return Err(SqlError::new(6903, format!("XML schema collection '{}' does not exist.", n)));
                };
                let used = self.srv.databases[&db].objects.values().any(|o| matches!(o, Object::Table(t) if t.columns.iter().any(|c| c.xml_schema.as_ref().is_some_and(|x| x.0 == db && x.1 == key))));
                if used {
                    return Err(SqlError::new(6906, format!("Cannot drop the XML schema collection '{}': a column is typed with it.", n)));
                }
                self.srv.cat_mut().databases.get_mut(&db).unwrap().xml_schemas.remove(i);
                self.save_xml_schemas(&db);
            }
        }
        Ok(())
    }

    /// The check that a typed xml column makes on a value that is stored in it.
    pub fn check_xml_store(&self, col: &Column, v: &Value) -> Result<()> {
        let Some((db, key, document)) = &col.xml_schema else { return Ok(()) };
        let Value::Str(s) = v else { return Ok(()) };
        let Some(coll) = self.srv.databases.get(db).and_then(|d| d.xml_schemas.iter().find(|x| obj_key(&x.schema, &x.name) == *key)) else {
            return Err(SqlError::new(6903, "The XML schema collection of this column does not exist."));
        };
        let c = compiled(&coll.texts).map_err(|m| SqlError::new(6905, format!("XML Schema: {}.", m)))?;
        let doc = crate::xmlpath::Doc::parse(s).ok_or_else(|| SqlError::new(9400, "XML parsing: the document is not well-formed."))?;
        c.validate(&doc, *document).map_err(|e| SqlError::new(e.number, e.message))
    }
}
