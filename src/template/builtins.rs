use std::borrow::Cow;

use minijinja::value::{Kwargs, Rest, ValueOrKwargs, from_args};
use minijinja::{Environment, Error, ErrorKind, State, Value, filters, tests};

use super::strict::{CYCLE, Undefined};

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
    environment.add_function(CYCLE, cycle);
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

// The loop object's `cycle` computes `index % args.len()`, which panics with no arguments. Only
// that object is intercepted, recognised by the exact key list it enumerates; a map's keys come
// sorted, so no map matches. Every other receiver's `cycle` method is called unchanged.
fn cycle(state: &mut State, receiver: &Value, args: &[Value]) -> Result<Value, Error> {
    if args.is_empty() && is_loop(receiver) {
        return Err(Error::new(
            ErrorKind::MissingArgument,
            "loop.cycle() needs at least one value to cycle through",
        ));
    }
    receiver.call_method(state, "cycle", args)
}

const LOOP_KEYS: [&str; 9] = [
    "index0",
    "index",
    "length",
    "revindex",
    "revindex0",
    "first",
    "last",
    "depth",
    "depth0",
];

fn is_loop(value: &Value) -> bool {
    value.as_object().is_some()
        && tests::is_mapping(value)
        && value.try_iter().is_ok_and(|keys| {
            keys.map(|key| key.as_str().map(str::to_owned))
                .eq(LOOP_KEYS.iter().map(|key| Some((*key).to_owned())))
        })
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
    let label = match (&named, positional.first()) {
        (Some(named), _) => format!("map(attribute=\"{named}\")"),
        (None, Some(filter)) => format!("map(\"{filter}\")"),
        (None, None) => "map()".to_owned(),
    };
    let mapped = filters::map(state, Value::from(items), args)?;
    if mapped.iter().any(Value::is_undefined) {
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
                "{filter}: the value of \"{key}\" is undefined"
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
                "map(attribute={key}): an item has no item {key}"
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
