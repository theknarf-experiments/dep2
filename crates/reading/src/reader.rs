/* ------------------------------------------------------------------------------------ */
/* I/O methods - Macro-based implementation for arities 1 through MAX_ARITY */
/* ------------------------------------------------------------------------------------ */

use std::fs::File;
use std::io::{BufRead, BufReader};

use crate::RecVariable;
use differential_dataflow::input::Input;
use differential_dataflow::input::InputSession;

use timely::dataflow::Scope;
use timely::order::Product;

use tracing::debug;

use crate::interner::encode_token;
use crate::rel::Rel;
use crate::row::Array;
use crate::row::FatRow;
use crate::row::Row;
use crate::semiring_one;
use crate::session::InputSessionGeneric;
use crate::Iter;
use crate::Semiring;
use crate::Time;
use parsing::decl::{DataType, RelDecl};

#[inline(always)]
pub fn reader(rel_path: &str) -> impl Iterator<Item = Vec<u8>> {
    let read_f = BufReader::new(
        File::open(rel_path).unwrap_or_else(|_| panic!("can't read data from \"{}\"", rel_path)),
    );

    read_f
        .split(b'\n')
        .filter_map(Result::ok) // filter out errors
        .filter(move |line| !line.is_empty()) // skip empty lines
}

/* ------------------------------------------------------------------------------------ */
/* read row for thin relations */
/* ------------------------------------------------------------------------------------ */
macro_rules! generate_read_row_functions {
    ($($n:expr),*) => {
        $(
            paste::paste! {
                pub fn [<read_row_ $n>](
                    rel_decl: &RelDecl,
                    rel_path: &str,
                    delimiter: &u8,
                    session: &mut InputSession<Time, Row<$n>, Semiring>,
                    id: usize,
                    peers: usize
                ) {
                    let rel_arity = rel_decl.arity();
                    // Per-column types drive the codec: `string` columns intern,
                    // `float` columns store IEEE bits, `number` columns parse i64.
                    let types: Vec<DataType> =
                        rel_decl.attributes().iter().map(|a| *a.data_type()).collect();

                    if id == 0 {
                        debug!("reading {} from {}", rel_decl, rel_path);
                    }

                    let ingest =
                        reader(rel_path)
                            .filter_map(move |line| {
                                let mut tuple = line.split(|&bt| bt == *delimiter);

                                let first_tok = std::str::from_utf8(tuple.next()?).ok()?;
                                let first_value = encode_token(first_tok, types[0])?;
                                if (first_value as usize) % peers != id {
                                    return None;
                                }

                                let mut row = Row::<$n>::new();
                                row.push(first_value);

                                let mut col = 1usize;
                                for value in tuple {
                                    let tok = std::str::from_utf8(value).ok()?;
                                    let dt = types.get(col).copied().unwrap_or(DataType::Integer);
                                    row.push(encode_token(tok, dt)?);
                                    col += 1;
                                }

                                if row.arity() != rel_arity {
                                    panic!("expected {} values, got {}", rel_arity, row.arity());
                                }

                                Some(row)
                            });

                    ingest.for_each(|row| session.update(row, semiring_one()));
                }
            }
        )*
    };
}

// `read_row_i(rel_decl: &RelDecl, rel_path: &str, delimiter: &u8, session: &mut InputSession<Time, Row<i>, Semiring>, id: usize, peers: usize)` for i from 1 to 8
generate_read_row_functions!(1, 2, 3, 4, 5, 6, 7, 8, 9, 10);

/* ------------------------------------------------------------------------------------ */
/* read row for fat relations */
/* ------------------------------------------------------------------------------------ */

pub fn read_row_fat(
    rel_decl: &RelDecl,
    rel_path: &str,
    delimiter: &u8,
    session: &mut InputSession<Time, FatRow, Semiring>,
    id: usize,
    peers: usize,
) {
    let rel_arity = rel_decl.arity();
    let types: Vec<DataType> = rel_decl
        .attributes()
        .iter()
        .map(|a| *a.data_type())
        .collect();

    let ingest = reader(rel_path).filter_map(move |line| {
        let mut tuple = line.split(|&bt| bt == *delimiter);

        let first_tok = std::str::from_utf8(tuple.next()?).ok()?;
        let first_value = encode_token(first_tok, types[0])?;
        if (first_value as usize) % peers != id {
            return None;
        }

        let mut row = FatRow::new();
        row.push(first_value);

        let mut col = 1usize;
        for value in tuple {
            let tok = std::str::from_utf8(value).ok()?;
            let dt = types.get(col).copied().unwrap_or(DataType::Integer);
            row.push(encode_token(tok, dt)?);
            col += 1;
        }

        if row.arity() != rel_arity {
            panic!("expected {} values, got {}", rel_arity, row.arity());
        }

        Some(row)
    });

    ingest.for_each(|row| session.update(row, semiring_one()));
}

/* ------------------------------------------------------------------------------------ */
/* construct session and table of some arity */
/* ------------------------------------------------------------------------------------ */

macro_rules! generate_construct_session_and_table {
    ($($n:expr),*) => {
        pub fn construct_session_and_table<'scope>(
            scope: Scope<'scope, Time>,
            arity: usize,
            fat_mode: bool,
        ) -> (InputSessionGeneric<Time>, Rel<'scope, Time>) {
            if !fat_mode {
                match arity {
                    $(
                        $n => {
                            let (session, input_rel) = scope.new_collection::<Row<$n>, Semiring>();
                            paste::paste! {
                                (InputSessionGeneric::[<InputSession $n>](session), Rel::[<Collection $n>](input_rel))
                            }
                        }
                    )*
                    _ => unreachable!("construct_session_and_table: arity {} overflows", arity),
                }
            } else {
                let (session, input_rel) = scope.new_collection::<FatRow, Semiring>();
                (
                    InputSessionGeneric::InputSessionFat(session, arity),
                    Rel::CollectionFat(input_rel, arity)
                )
            }
        }
    };
}

// `construct_session_and_table_i(scope: &mut G, arity: usize) -> (InputSessionGeneric<Time>, Rel<G>)` for i from 1 to 8
generate_construct_session_and_table!(1, 2, 3, 4, 5, 6, 7, 8);

/* ------------------------------------------------------------------------------------ */
/* read and insert row of some arity */
/* ------------------------------------------------------------------------------------ */

macro_rules! generate_read_row_generic {
    ($($n:expr),*) => {
        pub fn read_row_generic(
            rel_decl: &RelDecl,
            rel_path: &str,
            delimiter: &u8,
            session_generic: &mut InputSessionGeneric<Time>,
            id: usize,
            peers: usize,
            fat_mode: bool,
        ) {
            let arity = rel_decl.arity();
            if !fat_mode {
                match arity {
                    $(
                        $n => paste::paste! {
                            [<read_row_ $n>](rel_decl, rel_path, delimiter, &mut session_generic.[<listen_ $n>](), id, peers)
                        },
                    )*
                    _ => unreachable!("arity {} should be handled by match arms if <= MAX_ROW_ARITY", arity),
                }
            } else {
                // fat mode
                read_row_fat(rel_decl, rel_path, delimiter, &mut session_generic.listen_fat(), id, peers)
            }
        }
    };
}

// `read_row_generic(rel_decl: &RelDecl, rel_path: &str, delimiter: &u8, session_generic: &mut InputSessionGeneric<Time>, id: usize, peers: usize)` for i from 1 to 8
generate_read_row_generic!(1, 2, 3, 4, 5, 6, 7, 8);

/* ------------------------------------------------------------------------------------ */
/* construct semigroup variable of some arity */
/* ------------------------------------------------------------------------------------ */

macro_rules! generate_construct_var {
    ($($n:expr),*) => {
        pub fn construct_var<'scope>(
            scope: Scope<'scope, Product<Time, Iter>>,
            arity: usize,
            fat_mode: bool,
        ) -> Rel<'scope, Product<Time, Iter>> {
            if !fat_mode {
                match arity {
                    $(
                        $n => paste::paste! {{
                            // differential 0.20: Variable::new returns (handle, collection)
                            let (var, coll) = RecVariable::<_, Vec<(Row<$n>, Product<Time, Iter>, Semiring)>>::new(scope, Product::new(Default::default(), 1));
                            Rel::[<Variable $n>](var, coll)
                        }},
                    )*
                    _ => unreachable!("arity {} should be handled by match arms if <= MAX_ROW_ARITY", arity),
                }
            } else {
                // fat mode
                let (var, coll) = RecVariable::<_, Vec<(FatRow, Product<Time, Iter>, Semiring)>>::new(scope, Product::new(Default::default(), 1));
                Rel::VariableFat(var, coll, arity)
            }
        }
    };
}

// `construct_var_i(scope: &mut G, arity: usize) -> Rel<G>` for i from 1 to 8
generate_construct_var!(1, 2, 3, 4, 5, 6, 7, 8);

/* ------------------------------------------------------------------------------------ */
/* update session with a pre-encoded i64 row */
/* ------------------------------------------------------------------------------------ */

macro_rules! generate_update_session_generic {
    ($($n:expr),*) => {
        /// Feed a pre-encoded i64 slice into an `InputSessionGeneric`.
        pub fn update_session_generic(
            session: &mut InputSessionGeneric<Time>,
            row: &[i64],
            fat_mode: bool,
            diff: Semiring,
        ) {
            let arity = row.len();
            if !fat_mode {
                match arity {
                    $(
                        $n => {
                            let mut r = Row::<$n>::new();
                            for &v in row {
                                r.push(v);
                            }
                            paste::paste! {
                                session.[<listen_ $n>]().update(r, diff);
                            }
                        },
                    )*
                    _ => unreachable!("update_session_generic: arity {} overflows", arity),
                }
            } else {
                let mut r = FatRow::new();
                for &v in row {
                    r.push(v);
                }
                session.listen_fat().update(r, diff);
            }
        }
    };
}

generate_update_session_generic!(1, 2, 3, 4, 5, 6, 7, 8);
