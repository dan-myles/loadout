use minijinja::value::{Kwargs, Rest, ValueOrKwargs, from_args};
use minijinja::{Environment, Error, ErrorKind, State, Value, filters};

use super::strict::Undefined;

/// Replaces the builtins that would otherwise panic or consume a missing value out of sight.
pub(super) fn install(environment: &mut Environment<'_>) {
    environment.add_test("divisibleby", divisible_by);
    environment.add_filter("min", min);
    environment.add_filter("max", max);
    environment.add_filter("sort", sort);
    environment.add_filter("unique", unique);
    environment.add_filter("groupby", groupby);
    environment.add_filter("map", map);
}

// minijinja's own test computes `value % divisor` unchecked, which panics on a zero divisor.
fn divisible_by(value: &Value, divisor: &Value) -> Result<bool, Error> {
    let by_zero = || {
        Error::new(
            ErrorKind::InvalidOperation,
            "divisibleby(0) divides by zero",
        )
    };
    let integer = |value: &Value| i128::try_from(value.clone()).ok();
    let float = |value: &Value| f64::try_from(value.clone()).ok();
    if let (Some(value), Some(divisor)) = (integer(value), integer(divisor)) {
        return match value.checked_rem(divisor) {
            Some(remainder) => Ok(remainder == 0),
            None if divisor == 0 => Err(by_zero()),
            None => Ok(true),
        };
    }
    match (float(value), float(divisor)) {
        (Some(_), Some(0.0)) => Err(by_zero()),
        (Some(value), Some(divisor)) => Ok(value % divisor == 0.0),
        _ => Ok(false),
    }
}

// The attribute builtins below look each item's attribute up inside minijinja, where a missing
// one silently becomes a sort key, a group, or a list element. Each item must have every named
// attribute unless the filter's own `default` is given; then minijinja does the rest.

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
        Some(attribute) => Some(Value::from(attribute)),
        None => kwargs.peek::<Option<Value>>("attribute")?,
    };
    if let Some(named) = named
        && !kwargs.has("default")
    {
        require(&items, &named, "groupby")?;
    }
    filters::groupby(Value::from(items), attribute, kwargs)
}

fn map(state: &mut State, value: &Value, args: Rest<ValueOrKwargs>) -> Result<Vec<Value>, Error> {
    let items = listed(value)?;
    let values: Vec<Value> = args.iter().map(|arg| (**arg).clone()).collect();
    let (_, kwargs): (&[Value], Kwargs) = from_args(&values)?;
    if let Some(named) = kwargs.peek::<Option<Value>>("attribute")?
        && !kwargs.has("default")
    {
        require(&items, &named, "map")?;
    }
    filters::map(state, Value::from(items), args)
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
            require(&items, &Value::from(path), filter)?;
        }
    }
    Ok(Value::from(items))
}

fn listed(value: &Value) -> Result<Vec<Value>, Error> {
    Ok(value.try_iter()?.collect())
}

fn require(items: &[Value], attribute: &Value, filter: &str) -> Result<(), Error> {
    for item in items {
        let found = match attribute.as_str() {
            Some(path) => lookup(item, path),
            None => item.get_item(attribute).ok(),
        };
        if found.is_none_or(|found| found.is_undefined()) {
            let undefined = Undefined {
                expression: format!("{filter}(attribute={attribute})"),
                lookup: false,
                line: None,
            };
            return Err(Error::new(
                ErrorKind::UndefinedError,
                format!("{filter}(attribute={attribute:?}): an item has no `{attribute}`"),
            )
            .with_source(undefined));
        }
    }
    Ok(())
}

// The same walk as minijinja's private `Value::get_path`, which its attribute builtins use.
fn lookup(item: &Value, path: &str) -> Option<Value> {
    let mut value = item.clone();
    for part in path.split('.') {
        value = match part.parse::<usize>() {
            Ok(index) => value.get_item_by_index(index).ok()?,
            Err(_) => value.get_attr(part).ok()?,
        };
        if value.is_undefined() {
            return None;
        }
    }
    Some(value)
}
