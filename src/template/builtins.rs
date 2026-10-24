use std::borrow::Cow;
use std::collections::BTreeMap;

use minijinja::value::ValueKind;
use minijinja::value::{Kwargs, Rest, ValueOrKwargs, from_args};
use minijinja::{Environment, Error, ErrorKind, State, Value, filters, tests};

use super::strict::{ACYCLIC, CYCLE, STORE, UNPACK, Undefined, is_current_loop};

const ESCAPE_TESTS: [&str; 2] = ["defined", "undefined"];

/// Replaces the builtins that would otherwise panic, or let a missing value out of sight of the
/// template's guards. Together with `strict` they keep one invariant: a missing value is never
/// stored in a container and never consumed inside a builtin unless an explicit escape asked. So
/// the builtins that only move or compare elements (`select`, `reject`, `sum`, `join`, `chain`,
/// `zip`, `list`, `batch`, `slice`, `reverse`, ...) never see one and need no wrapper.
pub(super) fn install(environment: &mut Environment<'_>) {
    environment.add_test("divisibleby", divisible_by);
    environment.add_filter("min", min);
    environment.add_filter("max", max);
    environment.add_filter("sort", sort);
    environment.add_filter("unique", unique);
    environment.add_filter("groupby", groupby);
    environment.add_filter("map", map);
    environment.add_filter("selectattr", selectattr);
    environment.add_filter("rejectattr", rejectattr);
    environment.add_filter("items", items);
    environment.add_filter("dictsort", dictsort);
    environment.add_filter("chain", chain);
    environment.add_function(CYCLE, cycle);
    environment.add_function(ACYCLIC, acyclic);
    environment.add_function(UNPACK, unpack);
    environment.add_function(STORE, store);
}

// minijinja's test computes `a % b` unchecked on its integer path (neither operand a float), which
// panics for a zero divisor and for i128::MIN % -1. One rule for every numeric zero: when both
// operands are numbers or booleans, so a remainder would be computed, a divisor of 0, 0.0, or false
// is an error. Every other input keeps minijinja's own coercion and result.
fn divisible_by(value: &Value, divisor: &Value) -> Result<bool, Error> {
    let numeric = |value: &Value| tests::is_number(value) || tests::is_boolean(value);
    if numeric(value) && numeric(divisor) {
        let zero = if tests::is_float(divisor) {
            f64::try_from(divisor.clone()).is_ok_and(|divisor| divisor == 0.0)
        } else {
            i128::try_from(divisor.clone()).is_ok_and(|divisor| divisor == 0)
        };
        if zero {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                "divisibleby(0) divides by zero",
            ));
        }
        if !tests::is_float(value)
            && !tests::is_float(divisor)
            && let (Ok(value), Ok(divisor)) = (
                i128::try_from(value.clone()),
                i128::try_from(divisor.clone()),
            )
        {
            return Ok(value
                .checked_rem(divisor)
                .is_none_or(|remainder| remainder == 0));
        }
    }
    Ok(tests::is_divisibleby(value, divisor))
}

// The loop object's `cycle` computes `index % args.len()`, which panics with no arguments. With
// no arguments, only the running loop gets that error; a receiver with a `cycle` member keeps
// minijinja's call of that member; anything else gets minijinja's own unknown-method error, built
// here without calling into the receiver, because an aliased outer loop would still divide by
// zero. With arguments, every receiver is called unchanged.
fn cycle(state: &mut State, receiver: &Value, args: &[Value]) -> Result<Value, Error> {
    if !args.is_empty()
        || receiver
            .as_object()
            .is_some_and(|object| object.get_value(&Value::from("cycle")).is_some())
    {
        return receiver.call_method(state, "cycle", args);
    }
    if is_current_loop(state, receiver) {
        return Err(Error::new(
            ErrorKind::MissingArgument,
            "loop.cycle() needs at least one value to cycle through",
        ));
    }
    Err(Error::new(
        ErrorKind::UnknownMethod,
        format!("{} has no method named cycle", receiver.kind()),
    ))
}

// Every `{% set ns.x = value %}` passes through here between evaluating `ns` and storing. A value
// that reaches the receiver would make the namespace contain itself, and rendering, comparing, or
// sorting it would then recurse without end; so the store is refused before it happens.
fn acyclic(value: &Value, receiver: &Value, target: &str) -> Result<Value, Error> {
    if receiver.as_object().is_some() && reaches(value, receiver) {
        let owner = target.rsplit_once('.').map_or(target, |(owner, _)| owner);
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            format!("assigning to `{target}` would make `{owner}` contain itself"),
        ));
    }
    Ok(value.clone())
}

// minijinja's unpacking of a target list (`vm::unpack_list`): an object's items, checked, exactly
// `count` of them, with minijinja's own errors.
fn unpack(value: &Value, count: usize) -> Result<Value, Error> {
    if value.as_object().is_none() {
        return Err(Error::new(ErrorKind::CannotUnpack, "value is not iterable"));
    }
    let items = value
        .try_iter()?
        .checked()
        .collect::<Result<Vec<Value>, Error>>()?;
    if items.len() != count {
        return Err(Error::new(
            ErrorKind::CannotUnpack,
            format!(
                "sequence of wrong length (expected {count}, got {})",
                items.len()
            ),
        ));
    }
    Ok(Value::from(items))
}

// The snapshots of a set statement's target lists so far, plus list `number`'s items: a new map,
// so each list is unpacked exactly once and later targets read its items, not the live value.
fn store(known: &Value, number: usize, items: Value) -> Result<Value, Error> {
    let mut lists = BTreeMap::new();
    if !known.is_none() {
        for key in known.try_iter()? {
            let snapshot = known.get_item(&key)?;
            lists.insert(key, snapshot);
        }
    }
    lists.insert(Value::from(number), items);
    Ok(Value::from(lists))
}

// Walks what `start` holds by object identity only: no rendering, hashing, structural comparison,
// or call. Maps give their keys and values, sequences and iterables their items; every iterable a
// template can hold is re-iterable (minijinja's builtins make no one-shot iterators), and a plain
// object (a function) holds no template value. Each object is entered once.
fn reaches(start: &Value, target: &Value) -> bool {
    let mut pending = vec![start.clone()];
    let mut seen: Vec<Value> = Vec::new();
    while let Some(value) = pending.pop() {
        if value.as_object().is_none() {
            continue;
        }
        if tests::is_sameas(&value, target) {
            return true;
        }
        if seen.iter().any(|known| tests::is_sameas(known, &value)) {
            continue;
        }
        seen.push(value.clone());
        match value.kind() {
            ValueKind::Map => {
                if let Ok(keys) = value.try_iter() {
                    for key in keys {
                        if let Ok(item) = value.get_item(&key) {
                            pending.push(item);
                        }
                        pending.push(key);
                    }
                }
            }
            ValueKind::Seq | ValueKind::Iterable => {
                if let Ok(items) = value.try_iter() {
                    pending.extend(items);
                }
            }
            _ => {}
        }
    }
    false
}

// minijinja chains maps into a live view that hides which maps it reads, so a namespace behind it
// would be invisible to `reaches`. The maps are merged into an ordinary map instead, with the same
// precedence (the last map holding a key wins); it no longer follows later namespace changes.
fn chain(state: &State, value: &Value, others: Rest<Value>) -> Result<Value, Error> {
    let chained = filters::chain(state, value.clone(), others)?;
    if chained.kind() != ValueKind::Map {
        return Ok(chained);
    }
    let mut merged = BTreeMap::new();
    for key in chained.try_iter()? {
        let item = chained.get_item(&key)?;
        merged.insert(key, item);
    }
    Ok(Value::from(merged))
}

fn min(state: &State, value: &Value, kwargs: Kwargs) -> Result<Value, Error> {
    let items = attributes(value, &kwargs, "min", false)?;
    filters::min(state, items, kwargs)
}

fn max(state: &State, value: &Value, kwargs: Kwargs) -> Result<Value, Error> {
    let items = attributes(value, &kwargs, "max", false)?;
    filters::max(state, items, kwargs)
}

fn sort(state: &State, value: &Value, kwargs: Kwargs) -> Result<Value, Error> {
    let items = attributes(value, &kwargs, "sort", true)?;
    filters::sort(state, items, kwargs)
}

fn unique(state: &State, value: &Value, kwargs: Kwargs) -> Result<Value, Error> {
    let items = attributes(value, &kwargs, "unique", false)?;
    filters::unique(state, items, kwargs)
}

fn groupby(value: &Value, attribute: Option<&str>, kwargs: Kwargs) -> Result<Value, Error> {
    let items = listed(value)?;
    let named = match attribute {
        Some(attribute) => Some(attribute.to_owned()),
        None => kwargs.peek::<Option<String>>("attribute")?,
    };
    if let Some(named) = named {
        require(&items, &named, "groupby", kwargs.has("default"))?;
    }
    filters::groupby(Value::from(items), attribute, kwargs)
}

// Both modes are checked. With `attribute=`, each item needs the attribute unless `default=` is
// given. With a filter name, minijinja calls that filter on each item inside Rust, where no guard
// runs; an undefined result (`map("attr", "missing")`, `map("first")` on an empty item) is
// rejected before the list exists, so no later filter, test, or join can receive it.
fn map(state: &mut State, value: &Value, args: Rest<ValueOrKwargs>) -> Result<Vec<Value>, Error> {
    let items = listed(value)?;
    let values: Vec<Value> = args.iter().map(|arg| (**arg).clone()).collect();
    let (positional, kwargs): (&[Value], Kwargs) = from_args(&values)?;
    let named = kwargs.peek::<Option<Value>>("attribute")?;
    if let Some(named) = &named {
        match named.as_str() {
            Some(path) => require(&items, path, "map", kwargs.has("default"))?,
            None if !kwargs.has("default") => indexed(&items, named)?,
            None => {}
        }
    }
    let mapped = filters::map(state, Value::from(items), args)?;
    if mapped.iter().any(Value::is_undefined) {
        let label = match (&named, positional.first()) {
            (Some(named), _) => format!("map(attribute={})", shown(named)),
            (None, Some(filter)) => format!("map({})", shown(filter)),
            (None, None) => "map()".to_owned(),
        };
        return Err(undefined(format!("{label}: an item's result is undefined")));
    }
    Ok(mapped)
}

fn selectattr(
    state: &mut State,
    value: &Value,
    attribute: Cow<'_, str>,
    test: Option<Cow<'_, str>>,
    args: Rest<ValueOrKwargs>,
) -> Result<Vec<Value>, Error> {
    let items = attribute_test(value, &attribute, test.as_deref(), "selectattr")?;
    filters::selectattr(state, items, attribute, test, args)
}

fn rejectattr(
    state: &mut State,
    value: &Value,
    attribute: Cow<'_, str>,
    test: Option<Cow<'_, str>>,
    args: Rest<ValueOrKwargs>,
) -> Result<Vec<Value>, Error> {
    let items = attribute_test(value, &attribute, test.as_deref(), "rejectattr")?;
    filters::rejectattr(state, items, attribute, test, args)
}

// A missing attribute may reach only `defined` and `undefined`. Any other test, or plain
// truthiness, would decide on an undefined value.
fn attribute_test(
    value: &Value,
    attribute: &str,
    test: Option<&str>,
    filter: &str,
) -> Result<Value, Error> {
    let items = listed(value)?;
    let escaped = test.is_some_and(|test| ESCAPE_TESTS.contains(&test));
    require(&items, attribute, filter, escaped)?;
    Ok(Value::from(items))
}

// A map-like object can list a key it has no value for: the loop object lists `length` and
// `revindex` even when the loop's length is unknown.
fn items(value: &Value) -> Result<Value, Error> {
    let pairs = listed(&filters::items(value)?)?;
    paired(&pairs, "items")?;
    Ok(Value::from(pairs))
}

fn dictsort(value: &Value, kwargs: Kwargs) -> Result<Value, Error> {
    let sorted = filters::dictsort(value, kwargs)?;
    paired(&listed(&sorted)?, "dictsort")?;
    Ok(sorted)
}

fn paired(pairs: &[Value], filter: &str) -> Result<(), Error> {
    for pair in pairs {
        if pair.get_item_by_index(1)?.is_undefined() {
            let key = pair.get_item_by_index(0)?;
            return Err(undefined(format!(
                "{filter}: the value of {} is undefined",
                shown(&key)
            )));
        }
    }
    Ok(())
}

fn attributes(value: &Value, kwargs: &Kwargs, filter: &str, several: bool) -> Result<Value, Error> {
    let items = listed(value)?;
    if let Some(named) = kwargs.peek::<Option<&str>>("attribute")? {
        let paths: Vec<&str> = if several {
            named.split(',').map(str::trim).collect()
        } else {
            vec![named]
        };
        for path in paths {
            require(&items, path, filter, false)?;
        }
    }
    Ok(Value::from(items))
}

// Collecting once also turns a one-shot iterator into a list, so the check and the filter never
// consume the same input twice.
fn listed(value: &Value) -> Result<Vec<Value>, Error> {
    Ok(value.try_iter()?.collect())
}

// The same walk as minijinja's private `Value::get_path`. Every part before the last must exist,
// as for `a.b is defined`; the last may be missing only when the caller handles it explicitly.
fn require(items: &[Value], path: &str, filter: &str, final_optional: bool) -> Result<(), Error> {
    let parts: Vec<&str> = path.split('.').collect();
    for item in items {
        let mut value = item.clone();
        for (index, part) in parts.iter().enumerate() {
            value = match part.parse::<usize>() {
                Ok(position) => value.get_item_by_index(position),
                Err(_) => value.get_attr(part),
            }
            .unwrap_or(Value::UNDEFINED);
            let last = index + 1 == parts.len();
            if value.is_undefined() && !(last && final_optional) {
                let missing = parts[..=index].join(".");
                return Err(undefined(format!(
                    "{filter}(attribute={path:?}): an item has no `{missing}`"
                )));
            }
        }
    }
    Ok(())
}

fn indexed(items: &[Value], key: &Value) -> Result<(), Error> {
    for item in items {
        if item
            .get_item(key)
            .map_or(true, |found| found.is_undefined())
        {
            return Err(undefined(format!(
                "map(attribute={key}): an item has no item {key}",
                key = shown(key)
            )));
        }
    }
    Ok(())
}

fn undefined(message: String) -> Error {
    let source = Undefined {
        expression: message.clone(),
        lookup: false,
        line: None,
    };
    Error::new(ErrorKind::UndefinedError, message).with_source(source)
}

// Names a value in a message without rendering it: a string is quoted, anything else is its kind,
// so a deep or self-referring value can never make an error message recurse.
fn shown(value: &Value) -> String {
    match value.as_str() {
        Some(text) => format!("\"{text}\""),
        None => format!("a {}", value.kind()),
    }
}
