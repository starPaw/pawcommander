use crate::file_error::PermissionError;
use crate::operations::TransferError;

pub(super) const MODE_BITS: u32 = 0o7777;
const USER: u32 = 0o4700;
const GROUP: u32 = 0o2070;
const OTHER: u32 = 0o1007;
const EXECUTE: u32 = 0o111;

pub(crate) struct ModeRule(Mode);

enum Mode {
    Octal(u32),
    Symbolic(Vec<Action>),
}

struct Action {
    who: u32,
    operator: u8,
    permissions: Permissions,
}

enum Permissions {
    Bits {
        bits: u32,
        conditional_execute: bool,
    },
    Copy {
        shift: u32,
    },
}

impl ModeRule {
    pub(crate) fn parse(input: &str) -> Result<Self, TransferError> {
        parse(input.trim()).map_err(|()| PermissionError::InvalidRule.into())
    }

    pub(super) fn evaluate(&self, original: u32, is_dir: bool) -> u32 {
        let original = original & MODE_BITS;
        let conditional_execute = is_dir || original & EXECUTE != 0;
        match &self.0 {
            Mode::Octal(mode) => *mode,
            Mode::Symbolic(actions) => actions.iter().fold(original, |mode, action| {
                let bits = match action.permissions {
                    Permissions::Copy { shift } => ((mode >> shift) & 0o7) * 0o111,
                    Permissions::Bits {
                        bits,
                        conditional_execute: include,
                    } => {
                        bits | if include && conditional_execute {
                            EXECUTE
                        } else {
                            0
                        }
                    }
                } & action.who;
                match action.operator {
                    b'+' => mode | bits,
                    b'-' => mode & !bits,
                    b'=' => (mode & !action.who) | bits,
                    _ => unreachable!(),
                }
            }),
        }
    }
}

fn parse(input: &str) -> Result<ModeRule, ()> {
    if input.is_empty() || input.len() > 256 {
        return Err(());
    }
    if input.as_bytes()[0].is_ascii_digit() {
        if input.len() > 4 || !input.bytes().all(|byte| matches!(byte, b'0'..=b'7')) {
            return Err(());
        }
        return Ok(ModeRule(Mode::Octal(
            u32::from_str_radix(input, 8).map_err(|_| ())?,
        )));
    }

    let operators = ['+', '-', '='];
    let mut actions = Vec::new();
    for clause in input.split(',').map(str::trim) {
        let start = clause.find(operators).ok_or(())?;
        let who = if start == 0 {
            MODE_BITS
        } else {
            clause[..start].bytes().try_fold(0, |who, byte| {
                Ok(who
                    | match byte {
                        b'u' => USER,
                        b'g' => GROUP,
                        b'o' => OTHER,
                        b'a' => MODE_BITS,
                        _ => return Err(()),
                    })
            })?
        };
        let mut rest = &clause[start..];
        while !rest.is_empty() {
            let operator = rest.as_bytes()[0];
            let operands = &rest[1..];
            let end = operands.find(operators).unwrap_or(operands.len());
            actions.push(Action {
                who,
                operator,
                permissions: permissions(&operands[..end])?,
            });
            rest = &operands[end..];
        }
    }
    Ok(ModeRule(Mode::Symbolic(actions)))
}

fn permissions(input: &str) -> Result<Permissions, ()> {
    let shift = match input {
        "u" => 6,
        "g" => 3,
        "o" => 0,
        _ => {
            let mut bits = 0;
            let mut conditional_execute = false;
            for byte in input.bytes() {
                bits |= match byte {
                    b'r' => 0o444,
                    b'w' => 0o222,
                    b'x' => EXECUTE,
                    b'X' => {
                        conditional_execute = true;
                        0
                    }
                    b's' => 0o6000,
                    b't' => 0o1000,
                    _ => return Err(()),
                };
            }
            return Ok(Permissions::Bits {
                bits,
                conditional_execute,
            });
        }
    };
    Ok(Permissions::Copy { shift })
}

#[cfg(test)]
mod tests {
    use super::ModeRule;

    #[test]
    fn permissions_rules_use_original_execute_and_current_copy_permissions() {
        let cases = [
            ("0755", 0o104600, false, 0o755),
            ("0", 0o7777, false, 0),
            ("u+rw, go-w", 0o022, false, 0o600),
            ("g=u-w,o=", 0o765, false, 0o750),
            ("+x", 0o640, false, 0o751),
            ("a+X", 0o640, false, 0o640),
            ("a+X", 0o640, true, 0o751),
            ("u+x,g+X", 0, false, 0o100),
            ("u-x,g+X", 0o100, false, 0o010),
            ("u+s,g+s,o+t", 0o755, true, 0o7755),
            ("g=rx,o=", 0o7755, true, 0o4750),
        ];
        for (rule, before, directory, after) in cases {
            assert_eq!(
                ModeRule::parse(rule).unwrap().evaluate(before, directory),
                after,
                "{rule}"
            );
        }
    }

    #[test]
    fn permissions_rules_reject_malformed_modes() {
        for rule in [
            "", "08", "10000", "0o644", "755x", "ux", "z+r", "u+gu", "u+r,", "u＋r", "u+r /tmp",
        ] {
            assert!(ModeRule::parse(rule).is_err(), "{rule}");
        }
    }
}
