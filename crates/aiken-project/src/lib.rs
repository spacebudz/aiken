pub mod blueprint;
pub mod config;
pub mod deps;
pub mod docs;
pub mod error;
pub mod format;
pub mod module;
pub mod options;
pub mod package_name;
pub mod paths;
pub mod pretty;
pub mod script;
pub mod telemetry;

use crate::blueprint::{schema::Schema, validator, Blueprint};
use aiken_lang::{
    ast::{Definition, Function, ModuleKind, TypedDataType, TypedFunction},
    builder::{DataTypeKey, FunctionAccessKey},
    builtins,
    tipo::TypeInfo,
    IdGenerator,
};
use deps::UseManifest;
use indexmap::IndexMap;
use miette::NamedSource;
use options::{CodeGenMode, Options};
use package_name::PackageName;
use pallas::ledger::addresses::{
    Address, Network, ShelleyAddress, ShelleyDelegationPart, StakePayload,
};
use script::{EvalHint, EvalInfo, Script};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
};
use telemetry::EventListener;
use uplc::{
    ast::{Constant, DeBruijn, Term},
    machine::cost_model::ExBudget,
};

use crate::{
    config::Config,
    error::{Error, Warning},
    module::{CheckedModule, CheckedModules, ParsedModule, ParsedModules},
    telemetry::Event,
};

#[derive(Debug)]
pub struct Source {
    pub path: PathBuf,
    pub name: String,
    pub code: String,
    pub kind: ModuleKind,
}

pub struct Project<T>
where
    T: EventListener,
{
    config: Config,
    defined_modules: HashMap<String, PathBuf>,
    checked_modules: CheckedModules,
    id_gen: IdGenerator,
    module_types: HashMap<String, TypeInfo>,
    root: PathBuf,
    sources: Vec<Source>,
    pub warnings: Vec<Warning>,
    event_listener: T,
    functions: IndexMap<FunctionAccessKey, TypedFunction>,
    data_types: IndexMap<DataTypeKey, TypedDataType>,
}

impl<T> Project<T>
where
    T: EventListener,
{
    pub fn new(root: PathBuf, event_listener: T) -> Result<Project<T>, Error> {
        let id_gen = IdGenerator::new();

        let mut module_types = HashMap::new();

        module_types.insert("aiken".to_string(), builtins::prelude(&id_gen));
        module_types.insert("aiken/builtin".to_string(), builtins::plutus(&id_gen));

        let functions = builtins::prelude_functions(&id_gen);

        let data_types = builtins::prelude_data_types(&id_gen);

        let config = Config::load(&root)?;

        Ok(Project {
            config,
            checked_modules: CheckedModules::default(),
            defined_modules: HashMap::new(),
            id_gen,
            module_types,
            root,
            sources: vec![],
            warnings: vec![],
            event_listener,
            functions,
            data_types,
        })
    }

    pub fn build(&mut self, uplc: bool) -> Result<(), Error> {
        let options = Options {
            code_gen_mode: CodeGenMode::Build(uplc),
        };

        self.compile(options)
    }

    pub fn docs(&mut self, destination: Option<PathBuf>) -> Result<(), Error> {
        self.compile_deps()?;

        self.event_listener
            .handle_event(Event::BuildingDocumentation {
                root: self.root.clone(),
                name: self.config.name.to_string(),
                version: self.config.version.clone(),
            });

        self.read_source_files()?;

        let destination = destination.unwrap_or_else(|| self.root.join("docs"));

        let parsed_modules = self.parse_sources(self.config.name.clone())?;

        self.type_check(parsed_modules)?;

        self.event_listener.handle_event(Event::GeneratingDocFiles {
            output_path: destination.clone(),
        });

        let doc_files = docs::generate_all(
            &self.root,
            &self.config,
            self.checked_modules.values().collect(),
        );

        for file in doc_files {
            let path = destination.join(file.path);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(&path, file.content)?;
        }

        Ok(())
    }

    pub fn check(
        &mut self,
        skip_tests: bool,
        match_tests: Option<Vec<String>>,
        verbose: bool,
        exact_match: bool,
    ) -> Result<(), Error> {
        let options = Options {
            code_gen_mode: if skip_tests {
                CodeGenMode::NoOp
            } else {
                CodeGenMode::Test {
                    match_tests,
                    verbose,
                    exact_match,
                }
            },
        };

        self.compile(options)
    }

    pub fn dump_uplc(&self, blueprint: &Blueprint<Schema>) -> Result<(), Error> {
        let dir = self.root.join("artifacts");
        self.event_listener
            .handle_event(Event::DumpingUPLC { path: dir.clone() });
        fs::create_dir_all(&dir)?;
        for validator in &blueprint.validators {
            let path = dir
                .clone()
                .join(format!("{}::{}>.uplc", validator.title, validator.purpose));
            fs::write(&path, validator.program.to_pretty())
                .map_err(|error| Error::FileIo { error, path })?;
        }
        Ok(())
    }

    pub fn blueprint_path(&self) -> PathBuf {
        self.root.join("plutus.json")
    }

    pub fn compile(&mut self, options: Options) -> Result<(), Error> {
        self.compile_deps()?;

        self.event_listener
            .handle_event(Event::StartingCompilation {
                root: self.root.clone(),
                name: self.config.name.to_string(),
                version: self.config.version.clone(),
            });

        self.read_source_files()?;

        let parsed_modules = self.parse_sources(self.config.name.clone())?;

        self.type_check(parsed_modules)?;

        match options.code_gen_mode {
            CodeGenMode::Build(uplc_dump) => {
                self.event_listener
                    .handle_event(Event::GeneratingBlueprint {
                        path: self.blueprint_path(),
                    });

                let mut generator = self.checked_modules.new_generator(
                    &self.functions,
                    &self.data_types,
                    &self.module_types,
                );

                let blueprint = Blueprint::new(&self.config, &self.checked_modules, &mut generator)
                    .map_err(Error::Blueprint)?;

                if blueprint.validators.is_empty() {
                    self.warnings.push(Warning::NoValidators);
                }

                if uplc_dump {
                    self.dump_uplc(&blueprint)?;
                }

                let json = serde_json::to_string_pretty(&blueprint).unwrap();
                fs::write(self.blueprint_path(), json).map_err(|error| Error::FileIo {
                    error,
                    path: self.blueprint_path(),
                })
            }
            CodeGenMode::Test {
                match_tests,
                verbose,
                exact_match,
            } => {
                let tests = self.collect_tests(verbose)?;

                if !tests.is_empty() {
                    self.event_listener.handle_event(Event::RunningTests);
                }

                let results = self.eval_scripts(tests, match_tests, exact_match);

                let errors: Vec<Error> = results
                    .iter()
                    .filter_map(|e| {
                        if e.success {
                            None
                        } else {
                            Some(Error::TestFailure {
                                name: e.script.name.clone(),
                                path: e.script.input_path.clone(),
                                evaluation_hint: e.script.evaluation_hint.clone(),
                                src: e.script.program.to_pretty(),
                                verbose,
                            })
                        }
                    })
                    .collect();

                self.event_listener
                    .handle_event(Event::FinishedTests { tests: results });

                if !errors.is_empty() {
                    Err(Error::List(errors))
                } else {
                    Ok(())
                }
            }
            CodeGenMode::NoOp => Ok(()),
        }
    }

    pub fn address(
        &self,
        title: Option<&String>,
        purpose: Option<&validator::Purpose>,
        stake_address: Option<&String>,
    ) -> Result<ShelleyAddress, Error> {
        // Parse stake address
        let stake_address = stake_address
            .map(|s| {
                Address::from_hex(s)
                    .or_else(|_| Address::from_bech32(s))
                    .map_err(|error| Error::MalformedStakeAddress { error: Some(error) })
                    .and_then(|addr| match addr {
                        Address::Stake(addr) => Ok(addr),
                        _ => Err(Error::MalformedStakeAddress { error: None }),
                    })
            })
            .transpose()?;
        let delegation_part = match stake_address.map(|addr| addr.payload().to_owned()) {
            None => ShelleyDelegationPart::Null,
            Some(StakePayload::Stake(key)) => ShelleyDelegationPart::Key(key),
            Some(StakePayload::Script(script)) => ShelleyDelegationPart::Script(script),
        };

        // Read blueprint
        let blueprint = File::open(self.blueprint_path())
            .map_err(|_| blueprint::error::Error::InvalidOrMissingFile)?;
        let blueprint: Blueprint<serde_json::Value> =
            serde_json::from_reader(BufReader::new(blueprint))?;

        // Calculate the address
        let when_too_many =
            |known_validators| Error::MoreThanOneValidatorFound { known_validators };
        let when_missing = |known_validators| Error::NoValidatorNotFound { known_validators };
        blueprint.with_validator(title, purpose, when_too_many, when_missing, |validator| {
            let n = validator.parameters.len();
            if n > 0 {
                Err(blueprint::error::Error::ParameterizedValidator { n }.into())
            } else {
                Ok(validator
                    .program
                    .address(Network::Testnet, delegation_part.to_owned()))
            }
        })
    }

    pub fn apply_parameter(
        &self,
        title: Option<&String>,
        purpose: Option<&validator::Purpose>,
        param: &Term<DeBruijn>,
    ) -> Result<Blueprint<serde_json::Value>, Error> {
        // Read blueprint
        let blueprint = File::open(self.blueprint_path())
            .map_err(|_| blueprint::error::Error::InvalidOrMissingFile)?;
        let mut blueprint: Blueprint<serde_json::Value> =
            serde_json::from_reader(BufReader::new(blueprint))?;

        // Apply parameters
        let when_too_many =
            |known_validators| Error::MoreThanOneValidatorFound { known_validators };
        let when_missing = |known_validators| Error::NoValidatorNotFound { known_validators };
        let applied_validator =
            blueprint.with_validator(title, purpose, when_too_many, when_missing, |validator| {
                validator.apply(param).map_err(|e| e.into())
            })?;

        // Overwrite validator
        blueprint.validators = blueprint
            .validators
            .into_iter()
            .map(|validator| {
                let same_title = validator.title == applied_validator.title;
                let same_purpose = validator.purpose == applied_validator.purpose;
                if same_title && same_purpose {
                    applied_validator.to_owned()
                } else {
                    validator
                }
            })
            .collect();

        Ok(blueprint)
    }

    fn compile_deps(&mut self) -> Result<(), Error> {
        let manifest = deps::download(
            &self.event_listener,
            UseManifest::Yes,
            &self.root,
            &self.config,
        )?;

        for package in manifest.packages {
            let lib = self.root.join(paths::build_deps_package(&package.name));

            self.event_listener
                .handle_event(Event::StartingCompilation {
                    root: lib.clone(),
                    name: package.name.to_string(),
                    version: package.version.clone(),
                });

            self.read_package_source_files(&lib.join("lib"))?;

            let parsed_modules = self.parse_sources(package.name)?;

            self.type_check(parsed_modules)?;
        }

        Ok(())
    }

    fn read_source_files(&mut self) -> Result<(), Error> {
        let lib = self.root.join("lib");
        let validators = self.root.join("validators");

        self.aiken_files(&validators, ModuleKind::Validator)?;
        self.aiken_files(&lib, ModuleKind::Lib)?;

        Ok(())
    }

    fn read_package_source_files(&mut self, lib: &Path) -> Result<(), Error> {
        self.aiken_files(lib, ModuleKind::Lib)?;

        Ok(())
    }

    fn parse_sources(&mut self, package_name: PackageName) -> Result<ParsedModules, Error> {
        let mut errors = Vec::new();
        let mut parsed_modules = HashMap::with_capacity(self.sources.len());

        for Source {
            path,
            name,
            code,
            kind,
        } in self.sources.drain(0..)
        {
            match aiken_lang::parser::module(&code, kind) {
                Ok((mut ast, extra)) => {
                    // Store the name
                    ast.name = name.clone();

                    let mut module = ParsedModule {
                        kind,
                        ast,
                        code,
                        name,
                        path,
                        extra,
                        package: package_name.to_string(),
                    };

                    if let Some(first) = self
                        .defined_modules
                        .insert(module.name.clone(), module.path.clone())
                    {
                        return Err(Error::DuplicateModule {
                            module: module.name.clone(),
                            first,
                            second: module.path,
                        });
                    }

                    module.attach_doc_and_module_comments();

                    parsed_modules.insert(module.name.clone(), module);
                }
                Err(errs) => {
                    for error in errs {
                        errors.push(Error::Parse {
                            path: path.clone(),
                            src: code.clone(),
                            named: NamedSource::new(path.display().to_string(), code.clone()),
                            error: Box::new(error),
                        })
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(parsed_modules.into())
        } else {
            Err(Error::List(errors))
        }
    }

    fn type_check(&mut self, mut parsed_modules: ParsedModules) -> Result<(), Error> {
        let processing_sequence = parsed_modules.sequence()?;

        for name in processing_sequence {
            if let Some(ParsedModule {
                name,
                path,
                code,
                kind,
                extra,
                package,
                ast,
            }) = parsed_modules.remove(&name)
            {
                let mut type_warnings = Vec::new();

                let ast = ast
                    .infer(
                        &self.id_gen,
                        kind,
                        &self.config.name.to_string(),
                        &self.module_types,
                        &mut type_warnings,
                    )
                    .map_err(|error| Error::Type {
                        path: path.clone(),
                        src: code.clone(),
                        named: NamedSource::new(path.display().to_string(), code.clone()),
                        error,
                    })?;

                // Register any warnings emitted as type warnings
                let type_warnings = type_warnings
                    .into_iter()
                    .map(|w| Warning::from_type_warning(w, path.clone(), code.clone()));

                self.warnings.extend(type_warnings);

                // Register the types from this module so they can be imported into
                // other modules.
                self.module_types
                    .insert(name.clone(), ast.type_info.clone());

                self.checked_modules.insert(
                    name.clone(),
                    CheckedModule {
                        kind,
                        extra,
                        name,
                        code,
                        ast,
                        package,
                        input_path: path,
                    },
                );
            }
        }

        Ok(())
    }

    fn collect_tests(&mut self, verbose: bool) -> Result<Vec<Script>, Error> {
        let mut scripts = Vec::new();
        for module in self.checked_modules.values() {
            if module.package != self.config.name.to_string() {
                continue;
            }
            for def in module.ast.definitions() {
                if let Definition::Test(func) = def {
                    scripts.push((module.input_path.clone(), module.name.clone(), func))
                }
            }
        }

        let mut programs = Vec::new();
        for (input_path, module_name, func_def) in scripts {
            let Function {
                arguments,
                name,
                body,
                ..
            } = func_def;

            if verbose {
                self.event_listener.handle_event(Event::GeneratingUPLCFor {
                    name: name.clone(),
                    path: input_path.clone(),
                })
            }

            let mut generator = self.checked_modules.new_generator(
                &self.functions,
                &self.data_types,
                &self.module_types,
            );

            let evaluation_hint = if let Some((bin_op, left_src, right_src)) = func_def.test_hint()
            {
                let left = generator
                    .clone()
                    .generate(&left_src, &[], false)
                    .try_into()
                    .unwrap();

                let right = generator
                    .clone()
                    .generate(&right_src, &[], false)
                    .try_into()
                    .unwrap();

                Some(EvalHint {
                    bin_op,
                    left,
                    right,
                })
            } else {
                None
            };

            let program = generator.generate(body, arguments, false);

            let script = Script::new(
                input_path,
                module_name,
                name.to_string(),
                program.try_into().unwrap(),
                evaluation_hint,
            );

            programs.push(script);
        }

        Ok(programs)
    }

    fn eval_scripts(
        &self,
        scripts: Vec<Script>,
        match_tests: Option<Vec<String>>,
        exact_match: bool,
    ) -> Vec<EvalInfo> {
        use rayon::prelude::*;

        // TODO: in the future we probably just want to be able to
        // tell the machine to not explode on budget consumption.
        let initial_budget = ExBudget {
            mem: i64::MAX,
            cpu: i64::MAX,
        };

        let scripts = if let Some(match_tests) = match_tests {
            let match_tests: Vec<(&str, Option<Vec<String>>)> = match_tests
                .iter()
                .map(|match_test| {
                    let mut match_split_dot = match_test.split('.');

                    let match_module = if match_test.contains('.') || match_test.contains('/') {
                        match_split_dot.next().unwrap_or("")
                    } else {
                        ""
                    };

                    let match_names = match_split_dot.next().map(|names| {
                        let names = names.replace(&['{', '}'][..], "");

                        let names_split_comma = names.split(',');

                        names_split_comma.map(str::to_string).collect()
                    });

                    (match_module, match_names)
                })
                .collect();

            scripts
                .into_iter()
                .filter(|script| -> bool {
                    match_tests.iter().any(|(module, names)| {
                        let matched_module = module == &"" || script.module.contains(module);

                        let matched_name = match names {
                            None => true,
                            Some(names) => names.iter().any(|name| {
                                if exact_match {
                                    name == &script.name
                                } else {
                                    script.name.contains(name)
                                }
                            }),
                        };

                        matched_module && matched_name
                    })
                })
                .collect::<Vec<Script>>()
        } else {
            scripts
        };

        scripts
            .into_par_iter()
            .map(|script| match script.program.eval(initial_budget) {
                (Ok(result), remaining_budget, logs) => EvalInfo {
                    success: result != Term::Error
                        && result != Term::Constant(Constant::Bool(false).into()),
                    script,
                    spent_budget: initial_budget - remaining_budget,
                    output: Some(result),
                    logs,
                },
                (Err(..), remaining_budget, logs) => EvalInfo {
                    success: false,
                    script,
                    spent_budget: initial_budget - remaining_budget,
                    output: None,
                    logs,
                },
            })
            .collect()
    }

    fn aiken_files(&mut self, dir: &Path, kind: ModuleKind) -> Result<(), Error> {
        let paths = walkdir::WalkDir::new(dir)
            .follow_links(true)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(|d| d.into_path())
            .filter(move |d| is_aiken_path(d, dir));

        for path in paths {
            self.add_module(path, dir, kind)?;
        }

        Ok(())
    }

    fn add_module(&mut self, path: PathBuf, dir: &Path, kind: ModuleKind) -> Result<(), Error> {
        let name = self.module_name(dir, &path);
        let code = fs::read_to_string(&path).map_err(|error| Error::FileIo {
            path: path.clone(),
            error,
        })?;

        self.sources.push(Source {
            name,
            code,
            kind,
            path,
        });

        Ok(())
    }

    fn module_name(&self, package_path: &Path, full_module_path: &Path) -> String {
        // ../../{config.name}/module.ak

        // module.ak
        let mut module_path = full_module_path
            .strip_prefix(package_path)
            .expect("Stripping package prefix from module path")
            .to_path_buf();

        // module
        module_path.set_extension("");

        // Stringify
        let name = module_path
            .to_str()
            .expect("Module name path to str")
            .to_string();

        // normalise windows paths
        name.replace('\\', "/")
    }
}

fn is_aiken_path(path: &Path, dir: impl AsRef<Path>) -> bool {
    use regex::Regex;

    let re = Regex::new(&format!(
        "^({module}{slash})*{module}\\.ak$",
        module = "[a-z][_a-z0-9]*",
        slash = "(/|\\\\)",
    ))
    .expect("is_aiken_path() RE regex");

    re.is_match(
        path.strip_prefix(dir)
            .expect("is_aiken_path(): strip_prefix")
            .to_str()
            .expect("is_aiken_path(): to_str"),
    )
}
