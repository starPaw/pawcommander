use super::{MAX_INPUT, MAX_MODULE};
use crate::{
    i18n::text,
    preview::helper::{read_message, read_size, write_message},
};
use serde::{Deserialize, Serialize};
use std::io::{self, BufReader, BufWriter, Read, Write};
use wasmi::{
    Config, EnforcedLimits, Engine, ExternType, Instance, Module, Store, StoreLimitsBuilder,
};

pub(crate) const WORKER_ARG: &str = "--paw-viewer-worker";
const MAX_OUTPUT: usize = 16 * 1024;
const MAX_MEMORY: usize = 4 * 1024 * 1024;
const FUEL: u64 = 2_000_000;

#[derive(Serialize, Deserialize)]
pub(super) enum Request {
    Validate,
    Preview,
}

#[derive(Serialize, Deserialize)]
enum Problem {
    InvalidModule,
    Imports,
    Interface,
    ComputationLimit,
    Execution,
    InvalidText,
}

impl Problem {
    fn message(self) -> &'static str {
        match self {
            Self::InvalidModule => text!("Модуль просмотрщика повреждён или превышает лимиты."),
            Self::Imports => text!("Просмотрщик должен работать без внешних функций."),
            Self::Interface => text!("Модуль не поддерживает интерфейс просмотрщика версии 1."),
            Self::ComputationLimit => text!("Просмотрщик исчерпал лимит вычислений."),
            Self::Execution => text!("Просмотрщик завершился с ошибкой выполнения."),
            Self::InvalidText => text!("Просмотрщик вернул недопустимый текст."),
        }
    }
}

#[cfg(not(test))]
pub(super) fn invoke(request: Request, wasm: &[u8], input: &[u8]) -> Result<String, String> {
    use crate::{
        operations::Cancellation,
        preview::helper::{write_blob, Process},
    };
    use std::{process::Command, time::Duration};

    let cancel = Cancellation::default();
    let exchange = || -> io::Result<Result<String, Problem>> {
        let mut command = Command::new(std::env::current_exe()?);
        command.arg(WORKER_ARG).env_clear();
        let mut process = Process::spawn(&mut command)?;
        let (mut writer, mut reader) = process.pipes(Duration::from_secs(4), &cancel);
        write_message(&mut writer, &request)?;
        write_blob(&mut writer, wasm)?;
        write_blob(&mut writer, input)?;
        read_message(&mut reader)
    };
    match exchange() {
        Ok(reply) => reply.map_err(|problem| problem.message().into()),
        Err(error) if error.kind() == io::ErrorKind::TimedOut => {
            Err(text!("Просмотрщик не ответил вовремя.").into())
        }
        Err(_) => Err(text!("Процесс просмотрщика завершился без результата.").into()),
    }
}

#[cfg(test)]
pub(super) fn invoke(request: Request, wasm: &[u8], input: &[u8]) -> Result<String, String> {
    execute(request, wasm, input).map_err(|problem| problem.message().into())
}

pub(crate) fn run() -> i32 {
    let mut input = BufReader::new(io::stdin().lock());
    let mut output = BufWriter::new(io::stdout().lock());
    i32::from(serve(&mut input, &mut output).is_err())
}

fn serve(input: &mut impl Read, output: &mut impl Write) -> io::Result<()> {
    let request: Request = read_message(input)?;
    let wasm = read_blob(input, MAX_MODULE)?;
    let bytes = read_blob(input, MAX_INPUT)?;
    write_message(output, &execute(request, &wasm, &bytes))?;
    output.flush()
}

fn read_blob(input: &mut impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let size = read_size(input)?;
    if size > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut bytes = vec![0; size];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn execute(request: Request, wasm: &[u8], input: &[u8]) -> Result<String, Problem> {
    let mut config = Config::default();
    config
        .consume_fuel(true)
        .enforced_limits(EnforcedLimits::strict());
    let engine = Engine::new(&config);
    let module = Module::new(&engine, wasm).map_err(|_| Problem::InvalidModule)?;
    if module.imports().next().is_some() {
        return Err(Problem::Imports);
    }
    let valid_memory = matches!(module.get_export("memory"), Some(ExternType::Memory(memory))
        if !memory.is_64() && (1..=(MAX_MEMORY / 65536) as u64).contains(&memory.minimum()));
    use wasmi::ValType::{I32, I64};
    let valid_preview = matches!(module.get_export("paw_preview"), Some(ExternType::Func(function))
        if function.params() == [I32] && function.results() == [I64]);
    if !valid_memory || !valid_preview {
        return Err(Problem::Interface);
    }
    match request {
        Request::Validate => Ok(String::new()),
        Request::Preview => render(&engine, &module, input),
    }
}

fn render(engine: &Engine, module: &Module, input: &[u8]) -> Result<String, Problem> {
    let limits = StoreLimitsBuilder::new()
        .memory_size(MAX_MEMORY)
        .memories(1)
        .instances(1)
        .tables(1)
        .table_elements(512)
        .trap_on_grow_failure(true)
        .build();
    let mut store = Store::new(engine, limits);
    store.limiter(|limits| limits);
    store.set_fuel(FUEL).map_err(runtime_problem)?;
    let instance = Instance::new(&mut store, module, &[]).map_err(runtime_problem)?;
    let memory = instance
        .get_memory(&store, "memory")
        .ok_or(Problem::InvalidText)?;
    memory
        .write(&mut store, 0, input)
        .map_err(|_| Problem::InvalidText)?;
    let preview = instance
        .get_typed_func::<i32, i64>(&store, "paw_preview")
        .map_err(runtime_problem)?;
    let output = preview
        .call(&mut store, input.len() as i32)
        .map_err(runtime_problem)? as u64;
    let offset = (output >> 32) as usize;
    let len = output as u32 as usize;
    if len > MAX_OUTPUT {
        return Err(Problem::InvalidText);
    }
    let mut result = vec![0; len];
    memory
        .read(&store, offset, &mut result)
        .map_err(|_| Problem::InvalidText)?;
    let result = String::from_utf8(result).map_err(|_| Problem::InvalidText)?;
    Ok(result
        .chars()
        .filter(|&ch| matches!(ch, '\n' | '\t') || !ch.is_control())
        .collect())
}

fn runtime_problem(error: wasmi::Error) -> Problem {
    if error.as_trap_code() == Some(wasmi::TrapCode::OutOfFuel) {
        Problem::ComputationLimit
    } else {
        Problem::Execution
    }
}
