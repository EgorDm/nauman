use std::{
    io,
    fs::File,
    io::{BufReader, Read, Write},
    path::{PathBuf},
    process::Stdio,
    os::unix::io::{AsRawFd, FromRawFd},
};
use crate::{flow, flow::CommandId, logging::{MultiplexedOutput, OutputStream, DualOutputStream, DualWriter}, common::Env, pprint, logging};
use anyhow::{Context as AnyhowContext, Result};
use crate::config::LoggingConfig;
use crate::logging::{LoggingSpec, PipeSpec};
use colored::*;


#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub command_id: CommandId,
    pub exit_code: i32,
}

#[derive(Debug, Clone)]
pub struct ExecutionContext {
    pub env: Env,
    pub cwd: PathBuf,
    pub current: CommandId,
    pub previous: Option<ExecutionResult>,
}

pub fn resolve_cwd(current: &PathBuf, cwd: Option<&String>) -> PathBuf {
    let cwd = cwd.map(PathBuf::from).unwrap_or_else(|| current.clone());
    if cwd.is_absolute() {
        cwd
    } else {
        current.join(cwd)
    }
}

fn read_buffer(source: &mut BufReader<File>, buffer: &mut [u8]) -> io::Result<Option<usize>> {
    match source.read(buffer) {
        Ok(count) => Ok(Some(count)),
        Err(e) => match e.kind() {
            io::ErrorKind::WouldBlock => Ok(None),
            _ => Err(e),
        },
    }
}

const BUFFER_SIZE: usize = 1024; // 1 KB

pub fn capture_command(
    child: &std::process::Child,
    output: &mut DualOutputStream,
) -> Result<()> {
    // TODO: split into two functions
    let mut buffer = [0; BUFFER_SIZE];
    let (mut stdout_done, mut stderr_done) = (false, false);
    let mut stdout = BufReader::new(unsafe {
        File::from_raw_fd(child.stdout.as_ref().unwrap().as_raw_fd())
    });
    let mut stderr = BufReader::new(unsafe {
        File::from_raw_fd(child.stderr.as_ref().unwrap().as_raw_fd())
    });

    loop {
        match read_buffer(&mut stdout, &mut buffer) {
            Ok(None) => break,
            Ok(Some(size)) if size == 0 => {
                stdout_done = true;
            }
            Ok(Some(size)) => {
                output.write_stdout(&buffer[0..size]).unwrap();
            }
            Err(e) => return Err(e.into()),
        }

        match read_buffer(&mut stderr, &mut buffer) {
            Ok(None) => break,
            Ok(Some(size)) if size == 0 => {
                stderr_done = true;
            }
            Ok(Some(size)) => {
                output.write_stderr(&buffer[0..size]).unwrap();
            }
            Err(e) => return Err(e.into()),
        }

        if stderr_done && stdout_done {
            break;
        }
    }

    Ok(())
}

pub fn execute_command(
    command: &flow::Command,
    context: &mut ExecutionContext,
    output: &mut DualOutputStream,
) -> Result<ExecutionResult> {
    // Build env
    let mut env = context.env.clone();
    env.extend(command.env.clone());

    // Build cwd
    let cwd = resolve_cwd(&context.cwd, command.cwd.as_ref());

    // Build command
    let mut child = std::process::Command::new("sh")
        .args(&["-c", &command.run])
        .envs(env)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to execute command: {}", command.run))?;

    // Execute command, capture its output and return its exit code
    capture_command(&child, output)?;
    let exit_code = child.wait()?.code().unwrap_or(-1);

    Ok(ExecutionResult {
        command_id: context.current.clone(),
        exit_code,
    })
}

pub struct Executor {
    pub context: ExecutionContext,
}

impl Executor {
    pub fn new(context: ExecutionContext) -> Self {
        Executor { context }
    }

    pub fn execute(
        &mut self,
        command_id: &CommandId,
        command: &flow::Command,
        logging: &LoggingConfig,
    ) -> Result<ExecutionResult> {
        self.context.current = command_id.clone();

        let spec = LoggingSpec::from_config(logging, &self.context)?;
        let mut output = DualOutputStream::from_spec(spec);

        let result = execute_command(command, &mut self.context, &mut output)?;

        self.context.previous = Some(result.clone());
        Ok(result)
    }
}

pub fn execute_flow(
    flow: &flow::Flow,
    logging: &LoggingConfig,
) -> Result<Vec<ExecutionResult>> {
    let mut env: Env = std::env::vars().collect();
    env.extend(flow.env.clone());

    let cwd = resolve_cwd(&std::env::current_dir()?, flow.cwd.as_ref());

    let mut executor = Executor::new(ExecutionContext {
        env,
        cwd,
        current: CommandId::new(),
        previous: None,
    });

    let mut results = Vec::new();
    for (command_id, command) in flow.iter() {
        if command.is_hook {
            println!("{}", pprint::flex_banner(format!("Task: {}", &command.name)).yellow());
        } else {
            println!("{}", pprint::flex_banner(format!("Task: {}", &command.name)).green());
        }
        println!("{}", pprint::command(&command.run));

        let result = executor.execute(&command_id, &command, logging)?;
        results.push(result);
    }

    Ok(results)
}