/// this tools take 2 arguments, the content folder and the translation folder.
/// both do not need to be git repositories, the tool will use the nearest git repository.
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead};
use std::sync::LazyLock;
use std::{cmp, fmt, path, thread};

use cached::proc_macro::cached;
use clap::{Parser, Subcommand};
use git2::{Repository, TreeWalkMode, TreeWalkResult};
use log::{debug, error, info, log_enabled};
use pathdiff;
use rust_xlsxwriter as xlsx;

static CLI: LazyLock<Cli> = LazyLock::new(|| Cli::parse());
static MAX_CONCURRENT_WORKERS: LazyLock<usize> = LazyLock::new(|| match CLI.command {
    Commands::Compare {
        max_concurrent_workers: x,
        max_pack_capacity: _,
    } => x,
    // _ => panic!("max_concurrent_workers should be set in Compare command"),
});
static MAX_PACK_CAPACITY: LazyLock<usize> = LazyLock::new(|| match CLI.command {
    Commands::Compare {
        max_concurrent_workers: _,
        max_pack_capacity: x,
    } => x,
    // _ => panic!("max_pack_capacity should be set in Compare command"),
});

enum FileStatus {
    Unknown,
    PendingCommmit(String),
    Untranslated,
    NoSourceCommit,
    Behind(usize),
    Synced,
}

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// the path to the content folder, where the original files are
    #[arg(short, long)]
    content: path::PathBuf,

    /// the path to the translation folder, where the translation files are
    #[arg(short, long)]
    translation: path::PathBuf,

    /// log in the DEBUG level
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// compare content & translation folder, see the difference
    Compare {
        #[arg(long, default_value_t = 32)]
        max_concurrent_workers: usize,
        #[arg(long, default_value_t = 500)]
        max_pack_capacity: usize,
    },
}

impl fmt::Display for FileStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use FileStatus::*;
        let x: String = match self {
            Unknown => "Unknown".to_string(),
            PendingCommmit(c) => format!("Pending Commmit {c}"),
            Untranslated => "Untranslated".to_string(),
            NoSourceCommit => "No Source Commit".to_string(),
            Behind(d) => format!("Behind {}", d),
            Synced => "Synced".to_string(),
        };
        write!(f, "{}", x)
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
struct TranslationID {
    /// path of translated file, reletive to the provided translation folder
    /// note it is not `reletive path` mentioned in code, which is reletive to the nearest git repo
    slug: String,
}

impl TranslationID {
    fn new_with_reletive_path(path: String, base: Box<path::Path>) -> Option<Self> {
        let slug: String =
            pathdiff::diff_paths(get_nearest_repo_folder(base.clone()).join(path), base)? // TODO: optimize
                .to_str()
                .unwrap()
                .to_string();
        Some(Self { slug })
    }

    fn get_absolute_path(self: &Self) -> Box<path::Path> {
        get_translation_folder().join(self.slug.as_str()).into()
    }

    // 根据slug和lang获取翻译文件
    fn get_translation_file(self: &Self) -> Option<std::fs::File> {
        return std::fs::File::open(self.get_absolute_path()).ok();
    }
}

struct SingleResult {
    file_id: TranslationID,
    file_status: FileStatus,
}

#[inline]
#[cached]
fn get_content_folder() -> Box<path::Path> {
    let x = &CLI.content;
    log::debug!("content folder: {:?}", x);
    let x = x.canonicalize().unwrap();
    return x.into();
}

#[inline]
#[cached]
fn get_content_folder_reletive() -> Box<path::Path> {
    let x = pathdiff::diff_paths(
        get_content_folder(),
        get_nearest_repo_folder(get_content_folder()),
    )
    .unwrap();
    return x.into();
}

#[inline]
#[cached]
fn get_translation_folder() -> Box<path::Path> {
    let x = &CLI.translation;
    log::debug!("translation folder: {:?}", x);
    let x = x.canonicalize().unwrap();
    return x.into();
}

#[inline]
#[cached]
fn get_nearest_repo_folder(path: Box<path::Path>) -> Box<path::Path> {
    let mut path = path.to_path_buf();
    while !path.join(".git").exists() {
        path = path.parent().unwrap().to_path_buf();
    }
    path.into()
}

fn get_all_files(repo: &git2::Repository) -> Result<Vec<String>, git2::Error> {
    let tree = repo.head()?.peel_to_tree()?;
    let mut files = Vec::with_capacity(tree.len());

    let _ = tree
        .walk(TreeWalkMode::PreOrder, |root, entry| {
            let path = format!("{}{}", root, entry.name().unwrap_or(""));
            if let Some(git2::ObjectType::Blob) = entry.kind() {
                files.push(path);
            }
            TreeWalkResult::Ok
        })
        .unwrap();

    Ok(files)
}

fn get_tasks(content_repo: &git2::Repository) -> Vec<TranslationID> {
    get_all_files(content_repo)
        .unwrap()
        .iter()
        .filter(|x| x.ends_with(".txt") || x.ends_with(".md") || x.ends_with(".html"))
        .filter_map(|x| pathdiff::diff_paths(x, get_content_folder_reletive()))
        .map(|x| TranslationID {
            slug: x.into_os_string().into_string().unwrap(),
        })
        .collect()
}

fn get_status_by_id(t: &TranslationID) -> FileStatus {
    let pattern = regex::Regex::new(r"\s{1,}sourceCommit:\s?([0-9a-f]{40})").unwrap();
    let translation_file = match t.get_translation_file() {
        Some(x) => x,
        None => return FileStatus::Untranslated,
    };

    log::info!("reading file: {:?}", t.slug);
    let file_start = io::BufReader::new(translation_file)
        .lines()
        .take(7)
        .fold(String::new(), |acc, x| acc + x.unwrap().as_str());

    let source_commit = match pattern.captures(&file_start) {
        Some(x) => x.get(1).unwrap().as_str(),
        None => return FileStatus::NoSourceCommit,
    };

    return FileStatus::PendingCommmit(source_commit.into());
}

fn work(jobs: Vec<TranslationID>) -> Vec<SingleResult> {
    let mut result: Vec<SingleResult> = Vec::new();
    for i in jobs {
        result.push(SingleResult {
            file_status: get_status_by_id(&i),
            file_id: i,
        });
    }
    return result;
}

fn get_distances_by_commits(
    repo: &git2::Repository,
    target_commits: Vec<(&TranslationID, &String)>, // (slug, commit_id)
) -> Result<HashMap<(TranslationID, String), usize>, Box<dyn std::error::Error>> {
    let head_commit = repo.head()?.peel_to_commit()?;
    // let head_tree = head_commit.tree()?;

    let mut target_files: HashMap<&TranslationID, HashSet<&String>> =
        HashMap::with_capacity(target_commits.len()); // key is slug, value is commit ids
    for i in target_commits.iter() {
        let (slug, commit_id) = i;
        target_files
            .entry(&slug)
            .or_insert(HashSet::new())
            .insert(&commit_id);
    }
    let mut distances: HashMap<&TranslationID, usize> =
        HashMap::from_iter(target_commits.iter().map(|x| (x.0, 0)));
    let mut res: HashMap<(TranslationID, String), usize> =
        HashMap::with_capacity(target_commits.len());

    drop(target_commits);

    let mut curr_commit = head_commit;
    while !target_files.is_empty() {
        let curr_tree = curr_commit.tree()?;
        let curr_id = curr_commit.id().to_string();

        // the inital commit
        if curr_commit.parent_count() == 0 {
            curr_tree.walk(TreeWalkMode::PreOrder, |root, entry| {
                if let Some(name) = entry.name() {
                    let path = match TranslationID::new_with_reletive_path(
                        format!("{}{}", root, name),
                        get_content_folder(),
                    ) {
                        Some(x) => x,
                        None => return TreeWalkResult::Ok,
                    };
                    if target_files.contains_key(&path) {
                        res.insert((path, curr_id.clone()), usize::MAX);
                    }
                }
                TreeWalkResult::Ok
            })?;
            log::debug!("get_distances_by_commits have searched the inital commit");
            log::info!("target_files have {:?} item", target_files.len());
            break;
        }

        let parent = curr_commit.parent(0).unwrap();
        let parent_tree = parent.tree()?;
        let diff = repo.diff_tree_to_tree(Some(&parent_tree), Some(&curr_tree), None)?;

        for delta in diff.deltas() {
            if delta.status() == git2::Delta::Unmodified {
                continue;
            }
            // file_path is relative to the working directory of the repository
            let file_path = delta.new_file().path().or(delta.old_file().path());
            let file_path = file_path.unwrap();
            let slug = match TranslationID::new_with_reletive_path(
                file_path.to_str().unwrap().to_string(),
                get_content_folder(),
            ) {
                Some(x) => x,
                None => continue,
            };
            let ids_set = match target_files.get_mut(&slug) {
                Some(x) => x,
                None => continue,
            };

            use git2::Delta;
            if match delta.status() {
                Delta::Modified | Delta::Unmodified | Delta::Ignored => false,
                Delta::Added | Delta::Deleted | Delta::Renamed | Delta::Copied => {
                    log::error!(
                        "file {:?} was added, deleted, renamed or copied into current location in {}. I do not want to track more history of it.",
                        file_path,
                        curr_id
                    );
                    true
                }
                Delta::Conflicted | Delta::Unreadable | Delta::Untracked | Delta::Typechange => {
                    log::error!(
                        "file {:?} has undergone a change that I can not understand in {}, so I give up analysing it. This occasion is expected not to happen.",
                        file_path,
                        curr_id
                    );
                    true
                }
            } {
                target_files.remove(&slug);
                distances.remove(&slug);
                continue;
            }

            if ids_set.contains(&curr_id) {
                res.insert(
                    (slug.clone(), curr_id.clone()),
                    *distances.get(&slug).unwrap(),
                );
                ids_set.remove(&curr_id);
                if ids_set.is_empty() {
                    target_files.remove(&slug);
                    distances.remove(&slug);
                    continue;
                }
            }
            match distances.get_mut(&slug) {
                Some(x) => *x += 1,
                None => {}
            }
        }

        curr_commit = parent; // walk the tree
    }

    Ok(res)
}

fn export_results(results: &Vec<SingleResult>) -> Result<(), &'static str> {
    let mut wbook = xlsx::Workbook::new();
    let wsheet = wbook.add_worksheet();

    wsheet
        .write_row_matrix(
            0,
            0,
            results
                .iter()
                .map(|x| [x.file_id.slug.to_string(), x.file_status.to_string()]),
        )
        .unwrap();

    wsheet.autofit();
    wbook.save("output.xlsx").unwrap();
    Ok(())
}

fn feature_compare() {
    let mut workers: Vec<_> = Vec::new();
    let mut results_n: Vec<Vec<SingleResult>> = Vec::new(); // results_n: nested result

    debug!("the content folder is {:?}", get_content_folder());
    debug!("the translation folder is {:?}", get_translation_folder());
    let content_repo = Repository::init(get_nearest_repo_folder(get_content_folder())).unwrap();
    // let translation_repo = get_nearest_repo(&get_translation_folder());

    let mut tasks: Vec<TranslationID> = get_tasks(&content_repo);
    info!("got {} tasks.", tasks.len());

    while tasks.len() != 0 {
        let pack_capacity = cmp::min(*MAX_PACK_CAPACITY, tasks.len());
        let task_pack = tasks.split_off(tasks.len() - pack_capacity);
        let worker = thread::spawn(move || work(task_pack));
        debug!(
            "new worker: {:?}, with {pack_capacity} tasks. {} tasks left.",
            worker.thread().id(),
            tasks.len()
        );
        workers.push(worker);

        while workers.len() >= *MAX_CONCURRENT_WORKERS {
            if log_enabled!(log::Level::Error) && workers.len() > *MAX_CONCURRENT_WORKERS {
                error!(
                    "{} workers are in vec, greater than the maximum {}",
                    workers.len(),
                    *MAX_CONCURRENT_WORKERS
                );
            }
            thread::sleep(std::time::Duration::from_secs(1));

            let index_c = match workers.iter().position(|x| x.is_finished()) {
                Some(x) => x,
                None => continue,
            };
            debug!("finished worker: {:?}.", workers[index_c].thread().id());
            let worker_c = workers.swap_remove(index_c); // worker_c: current worker
            results_n.push(worker_c.join().unwrap());
        }
    }

    for w in workers {
        debug!("waiting for worker: {:?}.", w.thread().id());
        results_n.push(w.join().unwrap());
    }

    let mut result_f: Vec<_> = results_n.into_iter().flatten().collect();
    //distances_t: distances task
    let distances_t: Vec<(&TranslationID, &String)> = result_f
        .iter()
        .filter_map(|x| {
            if let FileStatus::PendingCommmit(c) = &x.file_status {
                Some((&x.file_id, c))
            } else {
                None
            }
        })
        .collect();
    debug!("searching commits.");
    let distances = get_distances_by_commits(&content_repo, distances_t).unwrap(); // distances: distances result
    for i in result_f.iter_mut() {
        if let FileStatus::PendingCommmit(c) = &i.file_status {
            i.file_status = distances.get(&(i.file_id.clone(), c.to_string())).map_or(
                FileStatus::Unknown,
                |x| {
                    if *x == 0 {
                        FileStatus::Synced
                    } else {
                        FileStatus::Behind(*x)
                    }
                },
            );
        }
    }
    export_results(&result_f).unwrap();
}

fn main() {
    if CLI.verbose {
        simple_logger::init_with_level(log::Level::Debug).unwrap();
    } else {
        simple_logger::init_with_level(log::Level::Info).unwrap();
    }

    if matches!(
        CLI.command,
        Commands::Compare {
            max_concurrent_workers: _,
            max_pack_capacity: _
        }
    ) {
        let _ = *MAX_CONCURRENT_WORKERS;
        let _ = *MAX_PACK_CAPACITY;
        log::info!("running compare command");
        feature_compare();
    }
}
