/*
 * Copyright Redis Ltd. 2016 - present
 * Licensed under your choice of the Redis Source Available License 2.0 (RSALv2) or
 * the Server Side Public License v1 (SSPLv1).
 */

use crate::error::Error;
use crate::manager::{err_json, err_msg_json_expected, err_msg_json_path_doesnt_exist};
use crate::manager::{Manager, ReadHolder, WriteHolder};
use crate::redisjson::normalize_arr_start_index;
use crate::Format;
use crate::REDIS_JSON_TYPE;
use bson::{from_document, Document};
use ijson::{DestructuredMut, INumber, IObject, IString, IValue, ValueType};
use json_path::select_value::{SelectValue, SelectValueType};
use redis_module::key::{verify_type, KeyFlags, RedisKey, RedisKeyWritable};
use redis_module::raw::{RedisModuleKey, Status};
use redis_module::rediserror::RedisError;
use redis_module::{Context, NotifyEvent, RedisResult, RedisString};
use serde::{Deserialize, Serialize};
use serde_json::Number;
use std::io::Cursor;
use std::marker::PhantomData;
use std::mem::size_of;

use crate::redisjson::RedisJSON;

use crate::array_index::ArrayIndex;

pub struct IValueKeyHolderWrite<'a> {
    key: RedisKeyWritable,
    key_name: RedisString,
    val: Option<&'a mut RedisJSON<IValue>>,
}

fn follow_path(path: Vec<String>, root: &mut IValue) -> Option<&mut IValue> {
    path.iter()
        .try_fold(root, |target, token| match target.destructure_mut() {
            DestructuredMut::Object(obj) => obj.get_mut(token.as_str()),
            DestructuredMut::Array(arr) => {
                let idx = token.parse::<usize>().expect(&format!(
                    "An array index is parsed successfully. Array = {:?}, index = {:?}",
                    arr, token
                ));
                arr.get_mut(idx)
            }
            _ => None,
        })
}

///
/// Updates a value at a given `path`, starting from `root`
///
/// The value is modified by `func`, which is called on the current value.
/// If the returned value from `func` is [`Err`], the current value remains (although it could be modified by `func`)
///
fn update<F, T>(path: Vec<String>, root: &mut IValue, func: F) -> RedisResult<T>
where
    F: FnMut(&mut IValue) -> Result<T, Error>,
{
    follow_path(path, root)
        .map_or_else(|| Err(err_msg_json_path_doesnt_exist().into()), func)
        .map_err(Into::into)
}

///
/// Removes a value at a given `path`, starting from `root`
///
fn remove(mut path: Vec<String>, root: &mut IValue) -> bool {
    let token = path.pop().unwrap();
    follow_path(path, root)
        .and_then(|target| match target.destructure_mut() {
            DestructuredMut::Object(obj) => obj.remove(token.as_str()),
            DestructuredMut::Array(arr) => {
                let idx = token.parse::<usize>().expect(&format!(
                    "An array index is parsed successfully. Array = {:?}, index = {:?}",
                    arr, token
                ));
                arr.remove(idx)
            }
            _ => None,
        })
        .is_some()
}

impl<'a> IValueKeyHolderWrite<'a> {
    fn do_op<F, T>(&mut self, paths: Vec<String>, op_fun: F) -> RedisResult<T>
    where
        F: FnMut(&mut IValue) -> Result<T, Error>,
    {
        self.get_value()
            .and_then(|root| update(paths, root.unwrap(), op_fun))
    }

    fn do_num_op<F1, F2>(
        &mut self,
        path: Vec<String>,
        num: &str,
        mut op1_fun: F1,
        mut op2_fun: F2,
    ) -> RedisResult<Number>
    where
        F1: FnMut(i64, i64) -> i64,
        F2: FnMut(f64, f64) -> f64,
    {
        let in_value = &serde_json::from_str(num)?;
        if let serde_json::Value::Number(in_value) = in_value {
            self.do_op(path, |v| {
                let num_res = match (v.get_type(), in_value.as_i64()) {
                    (SelectValueType::Long, Some(num2)) => {
                        let num1 = v.get_long();
                        let res = op1_fun(num1, num2);
                        Ok(res.into())
                    }
                    _ => {
                        let num1 = v.get_double();
                        let num2 = in_value.as_f64().unwrap();
                        INumber::try_from(op2_fun(num1, num2))
                            .map_err(|_| RedisError::Str("result is not a number"))
                    }
                };
                let new_val = IValue::from(num_res?);
                *v = new_val.clone();
                Ok(new_val)
            })
            .and_then(|n| {
                n.as_number()
                    .and_then(|n| {
                        if n.has_decimal_point() {
                            n.to_f64().and_then(serde_json::Number::from_f64)
                        } else {
                            n.to_i64().map(Into::into)
                        }
                    })
                    .ok_or_else(|| RedisError::Str("result is not a number"))
            })
        } else {
            Err(RedisError::Str("bad input number"))
        }
    }

    fn get_json_holder(&mut self) -> RedisResult<()> {
        if self.val.is_none() {
            self.val = self.key.get_value::<RedisJSON<IValue>>(&REDIS_JSON_TYPE)?;
        }
        Ok(())
    }

    fn set_root(&mut self, v: Option<IValue>) -> RedisResult<()> {
        if let Some(data) = v {
            self.get_json_holder()?;
            if let Some(val) = &mut self.val {
                val.data = data
            } else {
                self.key.set_value(&REDIS_JSON_TYPE, RedisJSON { data })?
            }
        } else {
            self.val = None;
            self.key.delete()?;
        }
        Ok(())
    }
}

impl<'a> WriteHolder<IValue, IValue> for IValueKeyHolderWrite<'a> {
    fn notify_keyspace_event(self, ctx: &Context, command: &str) -> RedisResult<()> {
        match ctx.notify_keyspace_event(NotifyEvent::MODULE, command, &self.key_name) {
            Status::Ok => Ok(()),
            Status::Err => Err(RedisError::Str("failed notify key space event")),
        }
    }

    fn delete(&mut self) -> RedisResult<()> {
        self.key.delete().and(Ok(()))
    }

    fn get_value(&mut self) -> RedisResult<Option<&mut IValue>> {
        self.get_json_holder()?;
        let val = self.val.as_mut().map(|v| &mut v.data);
        Ok(val)
    }

    fn set_value(&mut self, path: Vec<String>, mut v: IValue) -> RedisResult<bool> {
        if path.is_empty() {
            // update the root
            self.set_root(Some(v)).and(Ok(true))
        } else {
            self.get_value()
                .map(|root| update(path, root.unwrap(), |val| Ok(*val = v.take())).is_ok())
        }
    }

    fn merge_value(&mut self, path: Vec<String>, mut v: IValue) -> RedisResult<bool> {
        let root = self.get_value()?.unwrap();
        let updated = if path.is_empty() {
            // update the root
            merge(root, v);
            true
        } else {
            update(path, root, |current| Ok(merge(current, v.take()))).is_ok()
        };
        Ok(updated)
    }

    fn dict_add(&mut self, path: Vec<String>, key: &str, mut v: IValue) -> RedisResult<bool> {
        self.do_op(path, |val| {
            val.as_object_mut().map_or(Ok(false), |o| {
                let res = !o.contains_key(key);
                if res {
                    o.insert(key.to_string(), v.take());
                }
                Ok(res)
            })
        })
    }

    fn delete_path(&mut self, path: Vec<String>) -> RedisResult<bool> {
        self.get_value().map(|root| remove(path, root.unwrap()))
    }

    fn incr_by(&mut self, path: Vec<String>, num: &str) -> RedisResult<Number> {
        self.do_num_op(path, num, i64::wrapping_add, |f1, f2| f1 + f2)
    }

    fn mult_by(&mut self, path: Vec<String>, num: &str) -> RedisResult<Number> {
        self.do_num_op(path, num, i64::wrapping_mul, |f1, f2| f1 * f2)
    }

    fn pow_by(&mut self, path: Vec<String>, num: &str) -> RedisResult<Number> {
        self.do_num_op(path, num, |i1, i2| i1.pow(i2 as u32), f64::powf)
    }

    fn bool_toggle(&mut self, path: Vec<String>) -> RedisResult<bool> {
        self.do_op(path, |v| {
            if let DestructuredMut::Bool(mut bool_mut) = v.destructure_mut() {
                //Using DestructuredMut in order to modify a `Bool` variant
                let val = bool_mut.get() ^ true;
                bool_mut.set(val);
                Ok(val)
            } else {
                Err(err_json(v, "bool"))
            }
        })
    }

    fn str_append(&mut self, path: Vec<String>, val: String) -> RedisResult<usize> {
        match serde_json::from_str(&val)? {
            serde_json::Value::String(s) => self.do_op(path, |v| {
                v.as_string_mut()
                    .map(|v_str| {
                        let new_str = [v_str.as_str(), s.as_str()].concat();
                        *v_str = IString::intern(&new_str);
                        Ok(new_str.len())
                    })
                    .unwrap_or_else(|| Err(err_json(v, "string")))
            }),
            _ => Err(RedisError::String(err_msg_json_expected(
                "string",
                val.as_str(),
            ))),
        }
    }

    fn arr_append(&mut self, path: Vec<String>, args: &[IValue]) -> RedisResult<usize> {
        self.do_op(path, |v| {
            v.as_array_mut()
                .map(|arr| {
                    args.iter().for_each(|a| arr.push(a.clone()));
                    Ok(arr.len())
                })
                .unwrap_or_else(|| Err(err_json(v, "array")))
        })
    }

    fn arr_insert(
        &mut self,
        paths: Vec<String>,
        args: &[IValue],
        index: i64,
    ) -> RedisResult<usize> {
        self.do_op(paths, |v| {
            v.as_array_mut()
                .map(|arr| {
                    // Verify legal index in bounds
                    let len = arr.len() as _;
                    let index = if index < 0 { len + index } else { index };
                    if !(0..=len).contains(&index) {
                        return Err("ERR index out of bounds".into());
                    }
                    arr.extend(args.iter().map(IValue::clone));
                    arr.as_mut_slice()[index as _..].rotate_right(args.len());
                    Ok(arr.len())
                })
                .unwrap_or_else(|| Err(err_json(v, "array")))
        })
    }

    fn arr_pop<C>(&mut self, path: Vec<String>, index: i64, serialize_callback: C) -> RedisResult
    where
        C: FnOnce(Option<&IValue>) -> RedisResult,
    {
        self.do_op(path, |v| {
            v.as_array_mut()
                .map(|array| {
                    if array.is_empty() {
                        return None;
                    }
                    // Verify legal index in bounds
                    let len = array.len() as i64;
                    let index = normalize_arr_start_index(index, len) as usize;
                    array.remove(index)
                })
                .ok_or_else(|| err_json(v, "array"))
        })
        .and_then(|res| serialize_callback(res.as_ref()))
    }

    fn arr_trim(&mut self, path: Vec<String>, start: i64, stop: i64) -> RedisResult<usize> {
        self.do_op(path, |v| {
            v.as_array_mut()
                .map(|array| {
                    let len = array.len() as i64;
                    let stop = stop.normalize(len);
                    let start = if start < 0 || start < len {
                        start.normalize(len)
                    } else {
                        stop + 1 //  start >=0 && start >= len
                    };
                    let range = if start > stop || len == 0 {
                        0..0 // Return an empty array
                    } else {
                        start..(stop + 1)
                    };

                    array.rotate_left(range.start);
                    array.truncate(range.end - range.start);
                    array.len()
                })
                .ok_or_else(|| err_json(v, "array"))
        })
    }

    fn clear(&mut self, path: Vec<String>) -> RedisResult<usize> {
        self.do_op(path, |v| match v.destructure_mut() {
            DestructuredMut::Object(obj) => {
                obj.clear();
                Ok(1)
            }
            DestructuredMut::Array(arr) => {
                arr.clear();
                Ok(1)
            }
            DestructuredMut::Number(n) => {
                *n = INumber::from(0);
                Ok(1)
            }
            _ => Ok(0),
        })
    }
}

pub struct IValueKeyHolderRead {
    key: RedisKey,
}

impl ReadHolder<IValue> for IValueKeyHolderRead {
    fn get_value(&self) -> RedisResult<Option<&IValue>> {
        let data = self
            .key
            .get_value::<RedisJSON<IValue>>(&REDIS_JSON_TYPE)?
            .map(|v| &v.data);
        Ok(data)
    }
}

fn merge(doc: &mut IValue, mut patch: IValue) {
    if !patch.is_object() {
        *doc = patch;
        return;
    }

    if !doc.is_object() {
        *doc = IObject::new().into();
    }
    let map = doc.as_object_mut().unwrap();
    patch
        .as_object_mut()
        .unwrap()
        .into_iter()
        .for_each(|(key, value)| {
            if value.is_null() {
                map.remove(key.as_str());
            } else {
                merge(
                    map.entry(key.as_str()).or_insert(IValue::NULL),
                    value.take(),
                )
            }
        })
}

pub struct RedisIValueJsonKeyManager<'a> {
    pub phantom: PhantomData<&'a u64>,
}

impl<'a> Manager for RedisIValueJsonKeyManager<'a> {
    type WriteHolder = IValueKeyHolderWrite<'a>;
    type ReadHolder = IValueKeyHolderRead;
    type V = IValue;
    type O = IValue;

    fn open_key_read(&self, ctx: &Context, key: &RedisString) -> RedisResult<IValueKeyHolderRead> {
        let key = ctx.open_key(key);
        Ok(IValueKeyHolderRead { key })
    }

    fn open_key_read_with_flags(
        &self,
        ctx: &Context,
        key: &RedisString,
        flags: KeyFlags,
    ) -> RedisResult<Self::ReadHolder> {
        let key = ctx.open_key_with_flags(key, flags);
        Ok(IValueKeyHolderRead { key })
    }

    fn open_key_write(
        &self,
        ctx: &Context,
        key: RedisString,
    ) -> RedisResult<IValueKeyHolderWrite<'a>> {
        let key_ptr = ctx.open_key_writable(&key);
        Ok(IValueKeyHolderWrite {
            key: key_ptr,
            key_name: key,
            val: None,
        })
    }
    /**
     * This function is used to apply changes to the slave and AOF.
     * It is called after the command is executed.
     */
    fn apply_changes(&self, ctx: &Context) {
        ctx.replicate_verbatim();
    }

    fn from_str(&self, val: &str, format: Format, limit_depth: bool) -> Result<Self::O, Error> {
        match format {
            Format::JSON | Format::STRING => {
                let mut deserializer = serde_json::Deserializer::from_str(val);
                if !limit_depth {
                    deserializer.disable_recursion_limit();
                }
                IValue::deserialize(&mut deserializer).map_err(|e| e.into())
            }
            Format::BSON => from_document(
                Document::from_reader(&mut Cursor::new(val.as_bytes()))
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string().into())
            .and_then(|docs: Document| {
                docs.iter().next().map_or(Ok(IValue::NULL), |(_, b)| {
                    let v: serde_json::Value = b.clone().into();
                    let mut out = serde_json::Serializer::new(Vec::new());
                    v.serialize(&mut out).unwrap();
                    self.from_str(
                        &String::from_utf8(out.into_inner()).unwrap(),
                        Format::JSON,
                        limit_depth,
                    )
                })
            }),
        }
    }

    ///
    /// following https://github.com/Diggsey/ijson/issues/23#issuecomment-1377270111
    ///
    fn get_memory(&self, v: &Self::V) -> RedisResult<usize> {
        let res = size_of::<IValue>()
            + match v.type_() {
                ValueType::Null | ValueType::Bool => 0,
                ValueType::Number => {
                    let num = v.as_number().unwrap();
                    if num.has_decimal_point() {
                        // 64bit float
                        16
                    } else if num >= &INumber::from(-128) && num <= &INumber::from(383) {
                        // 8bit
                        0
                    } else if num > &INumber::from(-8_388_608) && num <= &INumber::from(8_388_607) {
                        // 24bit
                        4
                    } else {
                        // 64bit
                        16
                    }
                }
                ValueType::String => v.as_string().unwrap().len(),
                ValueType::Array => {
                    let arr = v.as_array().unwrap();
                    let capacity = arr.capacity();
                    if capacity == 0 {
                        0
                    } else {
                        size_of::<usize>() * (capacity + 2)
                            + arr
                                .into_iter()
                                .map(|v| self.get_memory(v).unwrap())
                                .sum::<usize>()
                    }
                }
                ValueType::Object => {
                    let val = v.as_object().unwrap();
                    let capacity = val.capacity();
                    if capacity == 0 {
                        0
                    } else {
                        size_of::<usize>() * (capacity * 3 + 2)
                            + val
                                .into_iter()
                                .map(|(s, v)| s.len() + self.get_memory(v).unwrap())
                                .sum::<usize>()
                    }
                }
            };
        Ok(res)
    }

    fn is_json(&self, key: *mut RedisModuleKey) -> RedisResult<bool> {
        Ok(verify_type(key, &REDIS_JSON_TYPE).is_ok())
    }
}

// a unit test for get_memory
#[cfg(test)]
mod tests {
    use super::*;

    static SINGLE_THREAD_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_get_memory() {
        let _guard = SINGLE_THREAD_TEST_MUTEX.lock();

        let manager = RedisIValueJsonKeyManager {
            phantom: PhantomData,
        };
        let json = r#"{
                            "a": 100.12,
                            "b": "foo",
                            "c": true,
                            "d": 126,
                            "e": -112,
                            "f": 7388608,
                            "g": -6388608,
                            "h": 9388608,
                            "i": -9485608,
                            "j": [],
                            "k": {},
                            "l": [1, "asas", {"a": 1}],
                            "m": {"t": "f"}
                        }"#;
        let value = serde_json::from_str(json).unwrap();
        let res = manager.get_memory(&value).unwrap();
        assert_eq!(res, 903);
    }

    /// Tests the deserialiser of IValue for a string with unicode
    /// characters, to ensure that the deserialiser can handle
    /// unicode characters well.
    #[test]
    fn test_unicode_characters() {
        let _guard = SINGLE_THREAD_TEST_MUTEX.lock();

        let json = r#""\u00a0\u00a0\u00a0\u00a0\u00a0\u00a0\u00a0""#;
        let value: IValue = serde_json::from_str(json).expect("IValue parses fine.");
        assert_eq!(
            value.as_string().unwrap(),
            "\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}"
        );

        let json = r#"{"\u00a0\u00a0\u00a0\u00a0\u00a0\u00a0\u00a0":"\u00a0\u00a0\u00a0\u00a0\u00a0\u00a0\u00a0"}"#;
        let value: IValue = serde_json::from_str(json).expect("IValue parses fine.");
        assert_eq!(
            value
                .as_object()
                .unwrap()
                .get("\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}")
                .unwrap()
                .as_string()
                .unwrap(),
            "\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}"
        );
    }
}
