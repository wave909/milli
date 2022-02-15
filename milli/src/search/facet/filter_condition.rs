use std::collections::HashSet;
use std::fmt::Debug;
use std::ops::Bound::{self, Excluded, Included};
use std::result::Result as StdResult;
use std::str::FromStr;

use either::Either;
use heed::types::DecodeIgnore;
use log::debug;
use pest::error::{Error as PestError, ErrorVariant};
use pest::iterators::{Pair, Pairs};
use pest::Parser;
use roaring::RoaringBitmap;

use self::FilterCondition::*;
use self::Operator::*;
use super::parser::{FilterParser, Rule, PREC_CLIMBER};
use super::FacetNumberRange;
use crate::error::FilterError;
use crate::heed_codec::facet::{
    FacetLevelValueF64Codec, FacetStringLevelZeroCodec, FacetStringLevelZeroValueCodec,
};
use crate::{
    distance_between_two_points, CboRoaringBitmapCodec, FieldId, FieldsIdsMap, Index, Result,
};

#[derive(Debug, Clone, PartialEq)]
pub enum Operator {
    GreaterThan(f64),
    GreaterThanOrEqual(f64),
    Equal(Option<f64>, String),
    NotEqual(Option<f64>, String),
    Includes(Option<f64>, String),
    NotIncludes(Option<f64>, String),
    LowerThan(f64),
    LowerThanOrEqual(f64),
    Between(f64, f64),
    GeoLowerThan([f64; 2], f64),
    GeoGreaterThan([f64; 2], f64),
}

impl Operator {
    /// This method can return two operations in case it must express
    /// an OR operation for the between case (i.e. `TO`).
    fn negate(self) -> (Self, Option<Self>) {
        match self {
            GreaterThan(n) => (LowerThanOrEqual(n), None),
            GreaterThanOrEqual(n) => (LowerThan(n), None),
            Equal(n, s) => (NotEqual(n, s), None),
            NotEqual(n, s) => (Equal(n, s), None),
            Includes(n, s) => (NotIncludes(n,s), None),
            NotIncludes(n, s) => (Includes(n,s), None),
            LowerThan(n) => (GreaterThanOrEqual(n), None),
            LowerThanOrEqual(n) => (GreaterThan(n), None),
            Between(n, m) => (LowerThan(n), Some(GreaterThan(m))),
            GeoLowerThan(point, distance) => (GeoGreaterThan(point, distance), None),
            GeoGreaterThan(point, distance) => (GeoLowerThan(point, distance), None),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FilterCondition {
    Operator(FieldId, Operator),
    Or(Box<Self>, Box<Self>),
    And(Box<Self>, Box<Self>),
    Empty,
}

impl FilterCondition {
    pub fn from_array<I, J, A, B>(
        rtxn: &heed::RoTxn,
        index: &Index,
        array: I,
    ) -> Result<Option<FilterCondition>>
    where
        I: IntoIterator<Item = Either<J, B>>,
        J: IntoIterator<Item = A>,
        A: AsRef<str>,
        B: AsRef<str>,
    {
        let mut ands = None;

        for either in array {
            match either {
                Either::Left(array) => {
                    let mut ors = None;
                    for rule in array {
                        let condition = FilterCondition::from_str(rtxn, index, rule.as_ref())?;
                        ors = match ors.take() {
                            Some(ors) => Some(Or(Box::new(ors), Box::new(condition))),
                            None => Some(condition),
                        };
                    }

                    if let Some(rule) = ors {
                        ands = match ands.take() {
                            Some(ands) => Some(And(Box::new(ands), Box::new(rule))),
                            None => Some(rule),
                        };
                    }
                }
                Either::Right(rule) => {
                    let condition = FilterCondition::from_str(rtxn, index, rule.as_ref())?;
                    ands = match ands.take() {
                        Some(ands) => Some(And(Box::new(ands), Box::new(condition))),
                        None => Some(condition),
                    };
                }
            }
        }

        Ok(ands)
    }

    pub fn from_str(
        rtxn: &heed::RoTxn,
        index: &Index,
        expression: &str,
    ) -> Result<FilterCondition> {
        let fields_ids_map = index.fields_ids_map(rtxn)?;
        let filterable_fields = index.filterable_fields(rtxn)?;
        let lexed = FilterParser::parse(Rule::prgm, expression).map_err(FilterError::Syntax)?;
        FilterCondition::from_pairs(&fields_ids_map, &filterable_fields, lexed)
    }

    fn from_pairs(
        fim: &FieldsIdsMap,
        ff: &HashSet<String>,
        expression: Pairs<Rule>,
    ) -> Result<Self> {
        PREC_CLIMBER.climb(
            expression,
            |pair: Pair<Rule>| match pair.as_rule() {
                Rule::greater => Ok(Self::greater_than(fim, ff, pair)?),
                Rule::geq => Ok(Self::greater_than_or_equal(fim, ff, pair)?),
                Rule::eq => Ok(Self::equal(fim, ff, pair)?),
                Rule::neq => Ok(Self::equal(fim, ff, pair)?.negate()),
                Rule::incl => Ok(Self::incl(fim, ff, pair)?),
                Rule::notincl => Ok(Self::incl(fim, ff, pair)?.negate()),
                Rule::leq => Ok(Self::lower_than_or_equal(fim, ff, pair)?),
                Rule::less => Ok(Self::lower_than(fim, ff, pair)?),
                Rule::between => Ok(Self::between(fim, ff, pair)?),
                Rule::geo_radius => Ok(Self::geo_radius(fim, ff, pair)?),
                Rule::not => Ok(Self::from_pairs(fim, ff, pair.into_inner())?.negate()),
                Rule::prgm => Self::from_pairs(fim, ff, pair.into_inner()),
                Rule::term => Self::from_pairs(fim, ff, pair.into_inner()),
                _ => unreachable!(),
            },
            |lhs: Result<Self>, op: Pair<Rule>, rhs: Result<Self>| match op.as_rule() {
                Rule::or => Ok(Or(Box::new(lhs?), Box::new(rhs?))),
                Rule::and => Ok(And(Box::new(lhs?), Box::new(rhs?))),
                _ => unreachable!(),
            },
        )
    }

    fn negate(self) -> FilterCondition {
        match self {
            Operator(fid, op) => match op.negate() {
                (op, None) => Operator(fid, op),
                (a, Some(b)) => Or(Box::new(Operator(fid, a)), Box::new(Operator(fid, b))),
            },
            Or(a, b) => And(Box::new(a.negate()), Box::new(b.negate())),
            And(a, b) => Or(Box::new(a.negate()), Box::new(b.negate())),
            Empty => Empty,
        }
    }

    fn geo_radius(
        fields_ids_map: &FieldsIdsMap,
        filterable_fields: &HashSet<String>,
        item: Pair<Rule>,
    ) -> Result<FilterCondition> {
        if !filterable_fields.contains("_geo") {
            return Err(FilterError::InvalidAttribute {
                field: "_geo".to_string(),
                valid_fields: filterable_fields.into_iter().cloned().collect(),
            }
            .into());
        }
        let mut items = item.into_inner();
        let fid = match fields_ids_map.id("_geo") {
            Some(fid) => fid,
            None => return Ok(Empty),
        };
        let parameters_item = items.next().unwrap();
        // We don't need more than 3 parameters, but to handle errors correctly we are still going
        // to extract the first 4 parameters
        let param_span = parameters_item.as_span();
        let parameters = parameters_item
            .into_inner()
            .take(4)
            .map(|param| (param.clone(), param.as_span()))
            .map(|(param, span)| pest_parse(param).0.map(|arg| (arg, span)))
            .collect::<StdResult<Vec<(f64, _)>, _>>()
            .map_err(FilterError::Syntax)?;
        if parameters.len() != 3 {
            return Err(FilterError::Syntax(PestError::new_from_span(
                        ErrorVariant::CustomError {
                            message: format!("The _geoRadius filter expect three arguments: _geoRadius(latitude, longitude, radius)"),
                        },
                        // we want to point to the last parameters and if there was no parameters we
                        // point to the parenthesis
                        parameters.last().map(|param| param.1.clone()).unwrap_or(param_span),
            )).into());
        }
        let (lat, lng, distance) = (&parameters[0], &parameters[1], parameters[2].0);
        if !(-90.0..=90.0).contains(&lat.0) {
            return Err(FilterError::Syntax(PestError::new_from_span(
                ErrorVariant::CustomError {
                    message: format!("Latitude must be contained between -90 and 90 degrees."),
                },
                lat.1.clone(),
            )))?;
        } else if !(-180.0..=180.0).contains(&lng.0) {
            return Err(FilterError::Syntax(PestError::new_from_span(
                ErrorVariant::CustomError {
                    message: format!("Longitude must be contained between -180 and 180 degrees."),
                },
                lng.1.clone(),
            )))?;
        }
        Ok(Operator(fid, GeoLowerThan([lat.0, lng.0], distance)))
    }

    fn between(
        fields_ids_map: &FieldsIdsMap,
        filterable_fields: &HashSet<String>,
        item: Pair<Rule>,
    ) -> Result<FilterCondition> {
        let mut items = item.into_inner();
        let fid = match field_id(fields_ids_map, filterable_fields, &mut items)? {
            Some(fid) => fid,
            None => return Ok(Empty),
        };

        let (lresult, _) = pest_parse(items.next().unwrap());
        let (rresult, _) = pest_parse(items.next().unwrap());

        let lvalue = lresult.map_err(FilterError::Syntax)?;
        let rvalue = rresult.map_err(FilterError::Syntax)?;

        Ok(Operator(fid, Between(lvalue, rvalue)))
    }

    fn equal(
        fields_ids_map: &FieldsIdsMap,
        filterable_fields: &HashSet<String>,
        item: Pair<Rule>,
    ) -> Result<FilterCondition> {
        let mut items = item.into_inner();
        let fid = match field_id(fields_ids_map, filterable_fields, &mut items)? {
            Some(fid) => fid,
            None => return Ok(Empty),
        };

        let value = items.next().unwrap();
        let (result, svalue) = pest_parse(value);

        let svalue = svalue.to_lowercase();
        Ok(Operator(fid, Equal(result.ok(), svalue)))
    }
    fn incl(
        fields_ids_map: &FieldsIdsMap,
        filterable_fields: &HashSet<String>,
        item: Pair<Rule>,
    ) -> Result<FilterCondition> {
        let mut items = item.into_inner();
        let fid = match field_id(fields_ids_map, filterable_fields, &mut items)? {
            Some(fid) => fid,
            None => return Ok(Empty),
        };

        let value = items.next().unwrap();
        let (result, svalue) = pest_parse(value);

        let svalue = svalue.to_lowercase();
        Ok(Operator(fid, Includes(result.ok(), svalue)))
    }

    fn greater_than(
        fields_ids_map: &FieldsIdsMap,
        filterable_fields: &HashSet<String>,
        item: Pair<Rule>,
    ) -> Result<FilterCondition> {
        let mut items = item.into_inner();
        let fid = match field_id(fields_ids_map, filterable_fields, &mut items)? {
            Some(fid) => fid,
            None => return Ok(Empty),
        };

        let value = items.next().unwrap();
        let (result, _svalue) = pest_parse(value);
        let value = result.map_err(FilterError::Syntax)?;

        Ok(Operator(fid, GreaterThan(value)))
    }

    fn greater_than_or_equal(
        fields_ids_map: &FieldsIdsMap,
        filterable_fields: &HashSet<String>,
        item: Pair<Rule>,
    ) -> Result<FilterCondition> {
        let mut items = item.into_inner();
        let fid = match field_id(fields_ids_map, filterable_fields, &mut items)? {
            Some(fid) => fid,
            None => return Ok(Empty),
        };

        let value = items.next().unwrap();
        let (result, _svalue) = pest_parse(value);
        let value = result.map_err(FilterError::Syntax)?;

        Ok(Operator(fid, GreaterThanOrEqual(value)))
    }

    fn lower_than(
        fields_ids_map: &FieldsIdsMap,
        filterable_fields: &HashSet<String>,
        item: Pair<Rule>,
    ) -> Result<FilterCondition> {
        let mut items = item.into_inner();
        let fid = match field_id(fields_ids_map, filterable_fields, &mut items)? {
            Some(fid) => fid,
            None => return Ok(Empty),
        };

        let value = items.next().unwrap();
        let (result, _svalue) = pest_parse(value);
        let value = result.map_err(FilterError::Syntax)?;

        Ok(Operator(fid, LowerThan(value)))
    }

    fn lower_than_or_equal(
        fields_ids_map: &FieldsIdsMap,
        filterable_fields: &HashSet<String>,
        item: Pair<Rule>,
    ) -> Result<FilterCondition> {
        let mut items = item.into_inner();
        let fid = match field_id(fields_ids_map, filterable_fields, &mut items)? {
            Some(fid) => fid,
            None => return Ok(Empty),
        };

        let value = items.next().unwrap();
        let (result, _svalue) = pest_parse(value);
        let value = result.map_err(FilterError::Syntax)?;

        Ok(Operator(fid, LowerThanOrEqual(value)))
    }
}

impl FilterCondition {
    /// Aggregates the documents ids that are part of the specified range automatically
    /// going deeper through the levels.
    fn explore_facet_number_levels(
        rtxn: &heed::RoTxn,
        db: heed::Database<FacetLevelValueF64Codec, CboRoaringBitmapCodec>,
        field_id: FieldId,
        level: u8,
        left: Bound<f64>,
        right: Bound<f64>,
        output: &mut RoaringBitmap,
    ) -> Result<()> {
        match (left, right) {
            // If the request is an exact value we must go directly to the deepest level.
            (Included(l), Included(r)) if l == r && level > 0 => {
                return Self::explore_facet_number_levels(
                    rtxn, db, field_id, 0, left, right, output,
                );
            }
            // lower TO upper when lower > upper must return no result
            (Included(l), Included(r)) if l > r => return Ok(()),
            (Included(l), Excluded(r)) if l >= r => return Ok(()),
            (Excluded(l), Excluded(r)) if l >= r => return Ok(()),
            (Excluded(l), Included(r)) if l >= r => return Ok(()),
            (_, _) => (),
        }

        let mut left_found = None;
        let mut right_found = None;

        // We must create a custom iterator to be able to iterate over the
        // requested range as the range iterator cannot express some conditions.
        let iter = FacetNumberRange::new(rtxn, db, field_id, level, left, right)?;

        debug!("Iterating between {:?} and {:?} (level {})", left, right, level);

        for (i, result) in iter.enumerate() {
            let ((_fid, level, l, r), docids) = result?;
            debug!("{:?} to {:?} (level {}) found {} documents", l, r, level, docids.len());
            *output |= docids;
            // We save the leftest and rightest bounds we actually found at this level.
            if i == 0 {
                left_found = Some(l);
            }
            right_found = Some(r);
        }

        // Can we go deeper?
        let deeper_level = match level.checked_sub(1) {
            Some(level) => level,
            None => return Ok(()),
        };

        // We must refine the left and right bounds of this range by retrieving the
        // missing part in a deeper level.
        match left_found.zip(right_found) {
            Some((left_found, right_found)) => {
                // If the bound is satisfied we avoid calling this function again.
                if !matches!(left, Included(l) if l == left_found) {
                    let sub_right = Excluded(left_found);
                    debug!(
                        "calling left with {:?} to {:?} (level {})",
                        left, sub_right, deeper_level
                    );
                    Self::explore_facet_number_levels(
                        rtxn,
                        db,
                        field_id,
                        deeper_level,
                        left,
                        sub_right,
                        output,
                    )?;
                }
                if !matches!(right, Included(r) if r == right_found) {
                    let sub_left = Excluded(right_found);
                    debug!(
                        "calling right with {:?} to {:?} (level {})",
                        sub_left, right, deeper_level
                    );
                    Self::explore_facet_number_levels(
                        rtxn,
                        db,
                        field_id,
                        deeper_level,
                        sub_left,
                        right,
                        output,
                    )?;
                }
            }
            None => {
                // If we found nothing at this level it means that we must find
                // the same bounds but at a deeper, more precise level.
                Self::explore_facet_number_levels(
                    rtxn,
                    db,
                    field_id,
                    deeper_level,
                    left,
                    right,
                    output,
                )?;
            }
        }

        Ok(())
    }

    fn evaluate_operator(
        rtxn: &heed::RoTxn,
        index: &Index,
        numbers_db: heed::Database<FacetLevelValueF64Codec, CboRoaringBitmapCodec>,
        strings_db: heed::Database<FacetStringLevelZeroCodec, FacetStringLevelZeroValueCodec>,
        field_id: FieldId,
        operator: &Operator,
    ) -> Result<RoaringBitmap> {
        // Make sure we always bound the ranges with the field id and the level,
        // as the facets values are all in the same database and prefixed by the
        // field id and the level.
        let (left, right) = match operator {
            GreaterThan(val) => (Excluded(*val), Included(f64::MAX)),
            GreaterThanOrEqual(val) => (Included(*val), Included(f64::MAX)),
            NotIncludes(number,val) => {
                let mut iter = strings_db.iter(rtxn)?;
                let mut result = RoaringBitmap::new();
                loop {
                    let cur = iter.next().transpose();
                    match cur {
                        Ok(Some(((fid,low_val),(data,docids)))) => {
                            if !low_val.contains(&val.to_lowercase()) {
                                for id in docids { result.insert(id); }
                            }
                        },
                        Ok(None) => { break; },
                        Err(E)=>{},
                    }
                }
                return Ok(result);
            }
            Includes(number,val) => { //TODO add number includence?
                let mut iter = strings_db.iter(rtxn)?;
                let mut result = RoaringBitmap::new();
                loop {
                    let cur = iter.next().transpose();
                    match cur {
                        Ok(Some(((fid,low_val),(data,docids)))) => {
                            if low_val.contains(&val.to_lowercase()) {
                                for id in docids { result.insert(id); }
                            }
                        },
                        Ok(None) => { break; },
                        Err(E)=>{},
                    }
                }
                return Ok(result);
            }
            Equal(number, string) => {
                let (_original_value, string_docids) =
                    strings_db.get(rtxn, &(field_id, &string))?.unwrap_or_default();
                let number_docids = match number {
                    Some(n) => {
                        let n = Included(*n);
                        let mut output = RoaringBitmap::new();
                        Self::explore_facet_number_levels(
                            rtxn,
                            numbers_db,
                            field_id,
                            0,
                            n,
                            n,
                            &mut output,
                        )?;
                        output
                    }
                    None => RoaringBitmap::new(),
                };
                return Ok(string_docids | number_docids);
            }
            NotEqual(number, string) => {
                let all_numbers_ids = if number.is_some() {
                    index.number_faceted_documents_ids(rtxn, field_id)?
                } else {
                    RoaringBitmap::new()
                };
                let all_strings_ids = index.string_faceted_documents_ids(rtxn, field_id)?;
                let operator = Equal(*number, string.clone());
                let docids = Self::evaluate_operator(
                    rtxn, index, numbers_db, strings_db, field_id, &operator,
                )?;
                return Ok((all_numbers_ids | all_strings_ids) - docids);
            }
            LowerThan(val) => (Included(f64::MIN), Excluded(*val)),
            LowerThanOrEqual(val) => (Included(f64::MIN), Included(*val)),
            Between(left, right) => (Included(*left), Included(*right)),
            GeoLowerThan(base_point, distance) => {
                let rtree = match index.geo_rtree(rtxn)? {
                    Some(rtree) => rtree,
                    None => return Ok(RoaringBitmap::new()),
                };

                let result = rtree
                    .nearest_neighbor_iter(base_point)
                    .take_while(|point| {
                        distance_between_two_points(base_point, point.geom()) < *distance
                    })
                    .map(|point| point.data)
                    .collect();

                return Ok(result);
            }
            GeoGreaterThan(point, distance) => {
                let result = Self::evaluate_operator(
                    rtxn,
                    index,
                    numbers_db,
                    strings_db,
                    field_id,
                    &GeoLowerThan(point.clone(), *distance),
                )?;
                let geo_faceted_doc_ids = index.geo_faceted_documents_ids(rtxn)?;
                return Ok(geo_faceted_doc_ids - result);
            }
        };

        // Ask for the biggest value that can exist for this specific field, if it exists
        // that's fine if it don't, the value just before will be returned instead.
        let biggest_level = numbers_db
            .remap_data_type::<DecodeIgnore>()
            .get_lower_than_or_equal_to(rtxn, &(field_id, u8::MAX, f64::MAX, f64::MAX))?
            .and_then(|((id, level, _, _), _)| if id == field_id { Some(level) } else { None });

        match biggest_level {
            Some(level) => {
                let mut output = RoaringBitmap::new();
                Self::explore_facet_number_levels(
                    rtxn,
                    numbers_db,
                    field_id,
                    level,
                    left,
                    right,
                    &mut output,
                )?;
                Ok(output)
            }
            None => Ok(RoaringBitmap::new()),
        }
    }

    pub fn evaluate(&self, rtxn: &heed::RoTxn, index: &Index) -> Result<RoaringBitmap> {
        let numbers_db = index.facet_id_f64_docids;
        let strings_db = index.facet_id_string_docids;

        match self {
            Operator(fid, op) => {
                Self::evaluate_operator(rtxn, index, numbers_db, strings_db, *fid, op)
            }
            Or(lhs, rhs) => {
                let lhs = lhs.evaluate(rtxn, index)?;
                let rhs = rhs.evaluate(rtxn, index)?;
                Ok(lhs | rhs)
            }
            And(lhs, rhs) => {
                let lhs = lhs.evaluate(rtxn, index)?;
                let rhs = rhs.evaluate(rtxn, index)?;
                Ok(lhs & rhs)
            }
            Empty => Ok(RoaringBitmap::new()),
        }
    }
}

/// Retrieve the field id base on the pest value.
///
/// Returns an error if the given value is not filterable.
///
/// Returns Ok(None) if the given value is filterable, but is not yet ascociated to a field_id.
///
/// The pest pair is simply a string associated with a span, a location to highlight in
/// the error message.
fn field_id(
    fields_ids_map: &FieldsIdsMap,
    filterable_fields: &HashSet<String>,
    items: &mut Pairs<Rule>,
) -> StdResult<Option<FieldId>, FilterError> {
    // lexing ensures that we at least have a key
    let key = items.next().unwrap();
    if key.as_rule() == Rule::reserved {
        return match key.as_str() {
            key if key.starts_with("_geoPoint") => {
                Err(FilterError::ReservedKeyword { field: "_geoPoint".to_string(), context: Some("Use the _geoRadius(latitude, longitude, distance) built-in rule to filter on _geo field coordinates.".to_string()) })
            }
            "_geo" => {
                Err(FilterError::ReservedKeyword { field: "_geo".to_string(), context: Some("Use the _geoRadius(latitude, longitude, distance) built-in rule to filter on _geo field coordinates.".to_string()) })
            }
            key =>
                Err(FilterError::ReservedKeyword { field: key.to_string(), context: None }),
        };
    }

    if !filterable_fields.contains(key.as_str()) {
        return Err(FilterError::InvalidAttribute {
            field: key.as_str().to_string(),
            valid_fields: filterable_fields.into_iter().cloned().collect(),
        });
    }

    Ok(fields_ids_map.id(key.as_str()))
}

/// Tries to parse the pest pair into the type `T` specified, always returns
/// the original string that we tried to parse.
///
/// Returns the parsing error associated with the span if the conversion fails.
fn pest_parse<T>(pair: Pair<Rule>) -> (StdResult<T, pest::error::Error<Rule>>, String)
where
    T: FromStr,
    T::Err: ToString,
{
    let result = match pair.as_str().parse::<T>() {
        Ok(value) => Ok(value),
        Err(e) => Err(PestError::<Rule>::new_from_span(
            ErrorVariant::CustomError { message: e.to_string() },
            pair.as_span(),
        )),
    };

    (result, pair.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use big_s::S;
    use heed::EnvOpenOptions;
    use maplit::hashset;

    use super::*;
    use crate::update::Settings;

    #[test]
    fn string() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // Set the filterable fields to be the channel.
        let mut wtxn = index.write_txn().unwrap();
        let mut map = index.fields_ids_map(&wtxn).unwrap();
        map.insert("channel");
        index.put_fields_ids_map(&mut wtxn, &map).unwrap();
        let mut builder = Settings::new(&mut wtxn, &index, 0);
        builder.set_filterable_fields(hashset! { S("channel") });
        builder.execute(|_, _| ()).unwrap();
        wtxn.commit().unwrap();

        // Test that the facet condition is correctly generated.
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_str(&rtxn, &index, "channel = Ponce").unwrap();
        let expected = Operator(0, Operator::Equal(None, S("ponce")));
        assert_eq!(condition, expected);

        let condition = FilterCondition::from_str(&rtxn, &index, "channel != ponce").unwrap();
        let expected = Operator(0, Operator::NotEqual(None, S("ponce")));
        assert_eq!(condition, expected);

        let condition = FilterCondition::from_str(&rtxn, &index, "NOT channel = ponce").unwrap();
        let expected = Operator(0, Operator::NotEqual(None, S("ponce")));
        assert_eq!(condition, expected);
    }

    #[test]
    fn number() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // Set the filterable fields to be the channel.
        let mut wtxn = index.write_txn().unwrap();
        let mut map = index.fields_ids_map(&wtxn).unwrap();
        map.insert("timestamp");
        index.put_fields_ids_map(&mut wtxn, &map).unwrap();
        let mut builder = Settings::new(&mut wtxn, &index, 0);
        builder.set_filterable_fields(hashset! { "timestamp".into() });
        builder.execute(|_, _| ()).unwrap();
        wtxn.commit().unwrap();

        // Test that the facet condition is correctly generated.
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_str(&rtxn, &index, "timestamp 22 TO 44").unwrap();
        let expected = Operator(0, Between(22.0, 44.0));
        assert_eq!(condition, expected);

        let condition = FilterCondition::from_str(&rtxn, &index, "NOT timestamp 22 TO 44").unwrap();
        let expected =
            Or(Box::new(Operator(0, LowerThan(22.0))), Box::new(Operator(0, GreaterThan(44.0))));
        assert_eq!(condition, expected);
    }

    #[test]
    fn parentheses() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // Set the filterable fields to be the channel.
        let mut wtxn = index.write_txn().unwrap();
        let mut builder = Settings::new(&mut wtxn, &index, 0);
        builder.set_searchable_fields(vec![S("channel"), S("timestamp")]); // to keep the fields order
        builder.set_filterable_fields(hashset! { S("channel"), S("timestamp") });
        builder.execute(|_, _| ()).unwrap();
        wtxn.commit().unwrap();

        // Test that the facet condition is correctly generated.
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_str(
            &rtxn,
            &index,
            "channel = gotaga OR (timestamp 22 TO 44 AND channel != ponce)",
        )
        .unwrap();
        let expected = Or(
            Box::new(Operator(0, Operator::Equal(None, S("gotaga")))),
            Box::new(And(
                Box::new(Operator(1, Between(22.0, 44.0))),
                Box::new(Operator(0, Operator::NotEqual(None, S("ponce")))),
            )),
        );
        assert_eq!(condition, expected);

        let condition = FilterCondition::from_str(
            &rtxn,
            &index,
            "channel = gotaga OR NOT (timestamp 22 TO 44 AND channel != ponce)",
        )
        .unwrap();
        let expected = Or(
            Box::new(Operator(0, Operator::Equal(None, S("gotaga")))),
            Box::new(Or(
                Box::new(Or(
                    Box::new(Operator(1, LowerThan(22.0))),
                    Box::new(Operator(1, GreaterThan(44.0))),
                )),
                Box::new(Operator(0, Operator::Equal(None, S("ponce")))),
            )),
        );
        assert_eq!(condition, expected);
    }

    #[test]
    fn reserved_field_names() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();
        let rtxn = index.read_txn().unwrap();

        assert!(FilterCondition::from_str(&rtxn, &index, "_geo = 12").is_err());

        assert!(FilterCondition::from_str(&rtxn, &index, r#"_geoDistance <= 1000"#).is_err());

        assert!(FilterCondition::from_str(&rtxn, &index, r#"_geoPoint > 5"#).is_err());

        assert!(FilterCondition::from_str(&rtxn, &index, r#"_geoPoint(12, 16) > 5"#).is_err());
    }

    #[test]
    fn geo_radius() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // Set the filterable fields to be the channel.
        let mut wtxn = index.write_txn().unwrap();
        let mut builder = Settings::new(&mut wtxn, &index, 0);
        builder.set_searchable_fields(vec![S("_geo"), S("price")]); // to keep the fields order
        builder.execute(|_, _| ()).unwrap();
        wtxn.commit().unwrap();

        let mut wtxn = index.write_txn().unwrap();
        let mut builder = Settings::new(&mut wtxn, &index, 0);
        builder.set_filterable_fields(hashset! { S("_geo"), S("price") });
        builder.execute(|_, _| ()).unwrap();
        wtxn.commit().unwrap();

        let rtxn = index.read_txn().unwrap();
        // basic test
        let condition =
            FilterCondition::from_str(&rtxn, &index, "_geoRadius(12, 13.0005, 2000)").unwrap();
        let expected = Operator(0, GeoLowerThan([12., 13.0005], 2000.));
        assert_eq!(condition, expected);

        // basic test with latitude and longitude at the max angle
        let condition =
            FilterCondition::from_str(&rtxn, &index, "_geoRadius(90, 180, 2000)").unwrap();
        let expected = Operator(0, GeoLowerThan([90., 180.], 2000.));
        assert_eq!(condition, expected);

        // basic test with latitude and longitude at the min angle
        let condition =
            FilterCondition::from_str(&rtxn, &index, "_geoRadius(-90, -180, 2000)").unwrap();
        let expected = Operator(0, GeoLowerThan([-90., -180.], 2000.));
        assert_eq!(condition, expected);

        // test the negation of the GeoLowerThan
        let condition =
            FilterCondition::from_str(&rtxn, &index, "NOT _geoRadius(50, 18, 2000.500)").unwrap();
        let expected = Operator(0, GeoGreaterThan([50., 18.], 2000.500));
        assert_eq!(condition, expected);

        // composition of multiple operations
        let condition = FilterCondition::from_str(
            &rtxn,
            &index,
            "(NOT _geoRadius(1, 2, 300) AND _geoRadius(1.001, 2.002, 1000.300)) OR price <= 10",
        )
        .unwrap();
        let expected = Or(
            Box::new(And(
                Box::new(Operator(0, GeoGreaterThan([1., 2.], 300.))),
                Box::new(Operator(0, GeoLowerThan([1.001, 2.002], 1000.300))),
            )),
            Box::new(Operator(1, LowerThanOrEqual(10.))),
        );
        assert_eq!(condition, expected);

        // georadius don't have any parameters
        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.to_string().contains(
            "The _geoRadius filter expect three arguments: _geoRadius(latitude, longitude, radius)"
        ));

        // georadius don't have any parameters
        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius()");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.to_string().contains(
            "The _geoRadius filter expect three arguments: _geoRadius(latitude, longitude, radius)"
        ));

        // georadius don't have enough parameters
        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius(1, 2)");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.to_string().contains(
            "The _geoRadius filter expect three arguments: _geoRadius(latitude, longitude, radius)"
        ));

        // georadius have too many parameters
        let result =
            FilterCondition::from_str(&rtxn, &index, "_geoRadius(1, 2, 3, 4, 5, 6, 7, 8, 9, 10)");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.to_string().contains(
            "The _geoRadius filter expect three arguments: _geoRadius(latitude, longitude, radius)"
        ));

        // georadius have a bad latitude
        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius(-100, 150, 10)");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error
            .to_string()
            .contains("Latitude must be contained between -90 and 90 degrees."));

        // georadius have a bad latitude
        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius(-90.0000001, 150, 10)");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error
            .to_string()
            .contains("Latitude must be contained between -90 and 90 degrees."));

        // georadius have a bad longitude
        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius(-10, 250, 10)");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error
            .to_string()
            .contains("Longitude must be contained between -180 and 180 degrees."));

        // georadius have a bad longitude
        let result = FilterCondition::from_str(&rtxn, &index, "_geoRadius(-10, 180.000001, 10)");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error
            .to_string()
            .contains("Longitude must be contained between -180 and 180 degrees."));
    }

    #[test]
    fn from_array() {
        let path = tempfile::tempdir().unwrap();
        let mut options = EnvOpenOptions::new();
        options.map_size(10 * 1024 * 1024); // 10 MB
        let index = Index::new(options, &path).unwrap();

        // Set the filterable fields to be the channel.
        let mut wtxn = index.write_txn().unwrap();
        let mut builder = Settings::new(&mut wtxn, &index, 0);
        builder.set_searchable_fields(vec![S("channel"), S("timestamp")]); // to keep the fields order
        builder.set_filterable_fields(hashset! { S("channel"), S("timestamp") });
        builder.execute(|_, _| ()).unwrap();
        wtxn.commit().unwrap();

        // Test that the facet condition is correctly generated.
        let rtxn = index.read_txn().unwrap();
        let condition = FilterCondition::from_array(
            &rtxn,
            &index,
            vec![
                Either::Right("channel = gotaga"),
                Either::Left(vec!["timestamp = 44", "channel != ponce"]),
            ],
        )
        .unwrap()
        .unwrap();
        let expected = FilterCondition::from_str(
            &rtxn,
            &index,
            "channel = gotaga AND (timestamp = 44 OR channel != ponce)",
        )
        .unwrap();
        assert_eq!(condition, expected);
    }
}
