use std::{fs, path::PathBuf, string::FromUtf8Error};

use glob::glob;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum FileLoaderError {
    #[error("Invalid glob pattern: {0}")]
    InvalidGlobPattern(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Pattern error: {0}")]
    PatternError(#[from] glob::PatternError),

    #[error("Glob error: {0}")]
    GlobError(#[from] glob::GlobError),

    #[error("String conversion error: {0}")]
    StringUtf8Error(#[from] FromUtf8Error),
}

// ================================================================
// Implementing Readable trait for reading file contents
// ================================================================
pub(crate) trait Readable {
    fn read(self) -> Result<String, FileLoaderError>;
    fn read_with_path(self) -> Result<(PathBuf, String), FileLoaderError>;
}

impl<'a> FileLoader<'a, PathBuf> {
    pub fn read(self) -> FileLoader<'a, Result<String, FileLoaderError>> {
        FileLoader {
            iterator: Box::new(self.iterator.map(|res| res.read())),
        }
    }
    pub fn read_with_path(self) -> FileLoader<'a, Result<(PathBuf, String), FileLoaderError>> {
        FileLoader {
            iterator: Box::new(self.iterator.map(|res| res.read_with_path())),
        }
    }
}

impl Readable for PathBuf {
    fn read(self) -> Result<String, FileLoaderError> {
        fs::read_to_string(self).map_err(FileLoaderError::IoError)
    }
    fn read_with_path(self) -> Result<(PathBuf, String), FileLoaderError> {
        let contents = fs::read_to_string(&self);
        Ok((self, contents?))
    }
}

impl Readable for Vec<u8> {
    fn read(self) -> Result<String, FileLoaderError> {
        Ok(String::from_utf8(self)?)
    }

    fn read_with_path(self) -> Result<(PathBuf, String), FileLoaderError> {
        let res = String::from_utf8(self)?;

        Ok((PathBuf::from("<memory>"), res))
    }
}

impl<T: Readable> Readable for Result<T, FileLoaderError> {
    fn read(self) -> Result<String, FileLoaderError> {
        self.map(|t| t.read())?
    }
    fn read_with_path(self) -> Result<(PathBuf, String), FileLoaderError> {
        self.map(|t| t.read_with_path())?
    }
}

// ================================================================
// FileLoader definitions and implementations
// ================================================================

/// [FileLoader] is a utility for loading files from the filesystem using glob patterns or directory
///  paths. It provides methods to read file contents and handle errors gracefully.
///
/// # Errors
///
/// This module defines a custom error type [FileLoaderError] which can represent various errors
///  that might occur during file loading operations, such as invalid glob patterns, IO errors, and
///  glob errors.
///
/// # Example Usage
///
/// ```no_run
/// use rig_core::loaders::FileLoader;
///
/// fn main() -> Result<(), Box<dyn std::error::Error>> {
///     // Create a FileLoader using a glob pattern
///     let loader = FileLoader::with_glob("path/to/files/*.txt")?;
///
///     // Read file contents, ignoring any errors
///     let contents: Vec<String> = loader
///         .read()
///         .ignore_errors()
///         .into_iter()
///         .collect();
///
///     for content in contents {
///         println!("{}", content);
///     }
///
///     Ok(())
/// }
/// ```
///
/// [FileLoader] uses strict typing between the iterator methods to ensure that transitions between
///   different implementations of the loaders and it's methods are handled properly by the compiler.
pub struct FileLoader<'a, T> {
    iterator: Box<dyn Iterator<Item = T> + 'a>,
}

impl<'a> FileLoader<'a, Result<PathBuf, FileLoaderError>> {
    /// Reads the contents of the files within the iterator returned by [FileLoader::with_glob] or
    ///  [FileLoader::with_dir].
    ///
    /// # Example
    /// Read files in directory "files/*.txt" and print the content for each file
    ///
    /// ```no_run
    /// # use rig_core::loaders::FileLoader;
    /// # fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let content = FileLoader::with_glob("files/*.txt")?.read();
    /// for result in content {
    ///     match result {
    ///         Ok(content) => println!("{}", content),
    ///         Err(e) => eprintln!("Error reading file: {}", e),
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn read(self) -> FileLoader<'a, Result<String, FileLoaderError>> {
        FileLoader {
            iterator: Box::new(self.iterator.map(|res| res.read())),
        }
    }
    /// Reads the contents of the files within the iterator returned by [FileLoader::with_glob] or
    ///  [FileLoader::with_dir] and returns the path along with the content.
    ///
    /// # Example
    /// Read files in directory "files/*.txt" and print the content for corresponding path for each
    ///  file.
    ///
    /// ```no_run
    /// # use rig_core::loaders::FileLoader;
    /// # fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let content = FileLoader::with_glob("files/*.txt")?.read_with_path();
    /// for result in content {
    ///     match result {
    ///         Ok((path, content)) => println!("{:?} {}", path, content),
    ///         Err(e) => eprintln!("Error reading file: {}", e),
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn read_with_path(self) -> FileLoader<'a, Result<(PathBuf, String), FileLoaderError>> {
        FileLoader {
            iterator: Box::new(self.iterator.map(|res| res.read_with_path())),
        }
    }
}

impl<'a, T> FileLoader<'a, Result<T, FileLoaderError>>
where
    T: 'a,
{
    /// Ignores errors in the iterator, returning only successful results. This can be used on any
    ///  [FileLoader] state of iterator whose items are results.
    ///
    /// # Example
    /// Read files in directory "files/*.txt" and ignore errors from unreadable files.
    ///
    /// ```no_run
    /// # use rig_core::loaders::FileLoader;
    /// # fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let content = FileLoader::with_glob("files/*.txt")?.read().ignore_errors();
    /// for content in content {
    ///     println!("{}", content)
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn ignore_errors(self) -> FileLoader<'a, T> {
        FileLoader {
            iterator: Box::new(self.iterator.filter_map(|res| res.ok())),
        }
    }
}

impl FileLoader<'_, Result<PathBuf, FileLoaderError>> {
    /// Creates a new [FileLoader] using a glob pattern to match files.
    ///
    /// # Example
    /// Create a [FileLoader] for all `.txt` files that match the glob "files/*.txt".
    ///
    /// ```no_run
    /// # use rig_core::loaders::FileLoader;
    /// # fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let loader = FileLoader::with_glob("files/*.txt")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_glob(
        pattern: &str,
    ) -> Result<FileLoader<'_, Result<PathBuf, FileLoaderError>>, FileLoaderError> {
        let paths = glob(pattern)?;
        Ok(FileLoader {
            iterator: Box::new(
                paths
                    .into_iter()
                    .map(|path| path.map_err(FileLoaderError::GlobError)),
            ),
        })
    }

    /// Creates a new [FileLoader] on all files within a directory.
    ///
    /// # Example
    /// Create a [FileLoader] for all files that are in the directory "files" (ignores subdirectories).
    ///
    /// ```no_run
    /// # use rig_core::loaders::FileLoader;
    /// # fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let loader = FileLoader::with_dir("files")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_dir(
        directory: &str,
    ) -> Result<FileLoader<'_, Result<PathBuf, FileLoaderError>>, FileLoaderError> {
        Ok(FileLoader {
            iterator: Box::new(fs::read_dir(directory)?.filter_map(|entry| {
                let path = entry.ok()?.path();
                if path.is_file() { Some(Ok(path)) } else { None }
            })),
        })
    }
}

impl<'a> FileLoader<'a, Vec<u8>> {
    /// Ingest a  as a byte array.
    pub fn from_bytes(bytes: Vec<u8>) -> FileLoader<'a, Vec<u8>> {
        FileLoader {
            iterator: Box::new(vec![bytes].into_iter()),
        }
    }

    /// Ingest multiple byte arrays.
    pub fn from_bytes_multi(bytes_vec: Vec<Vec<u8>>) -> FileLoader<'a, Vec<u8>> {
        FileLoader {
            iterator: Box::new(bytes_vec.into_iter()),
        }
    }

    /// Use this once you've created the loader to load the document in.
    pub fn read(self) -> FileLoader<'a, Result<String, FileLoaderError>> {
        FileLoader {
            iterator: Box::new(self.iterator.map(|res| res.read())),
        }
    }

    /// Use this once you've created the reader to load the document in (and get the path).
    pub fn read_with_path(self) -> FileLoader<'a, Result<(PathBuf, String), FileLoaderError>> {
        FileLoader {
            iterator: Box::new(self.iterator.map(|res| res.read_with_path())),
        }
    }
}

// ================================================================
// Iterators for FileLoader
// ================================================================

pub struct IntoIter<'a, T> {
    iterator: Box<dyn Iterator<Item = T> + 'a>,
}

impl<'a, T> IntoIterator for FileLoader<'a, T> {
    type Item = T;
    type IntoIter = IntoIter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            iterator: self.iterator,
        }
    }
}

impl<T> Iterator for IntoIter<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.iterator.next()
    }
}

#[cfg(test)]
mod tests {
    use assert_fs::fixture::PathCreateDir;
    use assert_fs::prelude::{FileTouch, FileWriteStr, PathChild};

    use super::FileLoader;

    #[test]
    fn test_file_loader() {
        let temp = assert_fs::TempDir::new().expect("Failed to create temp dir");
        let foo_file = temp.child("foo.txt");
        let bar_file = temp.child("bar.txt");

        foo_file.touch().expect("Failed to create foo.txt");
        bar_file.touch().expect("Failed to create bar.txt");

        foo_file.write_str("foo").expect("Failed to write to foo");
        bar_file.write_str("bar").expect("Failed to write to bar");

        let glob = temp.path().to_string_lossy().to_string() + "/*.txt";

        let loader = FileLoader::with_glob(&glob).unwrap();
        let mut actual = loader
            .ignore_errors()
            .read()
            .ignore_errors()
            .into_iter()
            .collect::<Vec<_>>();
        let mut expected = vec!["foo".to_string(), "bar".to_string()];

        actual.sort();
        expected.sort();

        assert!(!actual.is_empty());
        assert!(expected == actual)
    }

    #[test]
    fn test_file_loader_bytes() {
        let temp = assert_fs::TempDir::new().expect("Failed to create temp dir");
        let foo_file = temp.child("foo.txt");
        let bar_file = temp.child("bar.txt");

        foo_file.touch().expect("Failed to create foo.txt");
        bar_file.touch().expect("Failed to create bar.txt");

        foo_file.write_str("foo").expect("Failed to write to foo");
        bar_file.write_str("bar").expect("Failed to write to bar");

        let foo_bytes = std::fs::read(foo_file.path()).unwrap();
        let bar_bytes = std::fs::read(bar_file.path()).unwrap();

        let loader = FileLoader::from_bytes_multi(vec![foo_bytes, bar_bytes]);
        let mut actual = loader
            .read()
            .ignore_errors()
            .into_iter()
            .collect::<Vec<_>>();
        let mut expected = vec!["foo".to_string(), "bar".to_string()];

        actual.sort();
        expected.sort();

        assert!(!actual.is_empty());
        assert!(expected == actual)
    }

    #[test]
    fn test_file_loader_with_dir_reads_only_files() {
        let temp = assert_fs::TempDir::new().expect("Failed to create temp dir");
        let foo_file = temp.child("foo.txt");
        let bar_file = temp.child("bar.txt");
        let nested_dir = temp.child("nested");

        foo_file.write_str("foo").expect("Failed to write to foo");
        bar_file.write_str("bar").expect("Failed to write to bar");
        nested_dir
            .create_dir_all()
            .expect("Failed to create nested dir");
        nested_dir
            .child("ignored.txt")
            .write_str("ignored")
            .expect("Failed to write nested file");

        let dir = temp.path().to_string_lossy();
        let loader = FileLoader::with_dir(dir.as_ref()).expect("with_dir failed");
        let mut actual: Vec<String> = loader
            .read()
            .ignore_errors()
            .into_iter()
            .collect::<Vec<_>>();
        actual.sort();

        assert_eq!(actual, vec!["bar".to_string(), "foo".to_string()]);
    }

    #[test]
    fn test_file_loader_with_dir_missing_directory_errors() {
        let temp = assert_fs::TempDir::new().expect("Failed to create temp dir");
        let missing = temp.path().join("does-not-exist");

        let error = FileLoader::with_dir(missing.to_string_lossy().as_ref())
            .err()
            .expect("with_dir on a missing directory should fail");
        assert!(matches!(error, super::FileLoaderError::IoError(_)));
    }

    #[test]
    fn test_file_loader_read_with_path_returns_path_and_contents() {
        let temp = assert_fs::TempDir::new().expect("Failed to create temp dir");
        let foo_file = temp.child("foo.txt");
        foo_file.write_str("foo").expect("Failed to write to foo");

        let glob = temp.path().to_string_lossy().to_string() + "/foo.txt";

        let loader = FileLoader::with_glob(&glob).expect("with_glob failed");
        let actual: Vec<(std::path::PathBuf, String)> = loader
            .read_with_path()
            .ignore_errors()
            .into_iter()
            .collect();

        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].0, foo_file.path());
        assert_eq!(actual[0].1, "foo");
    }

    #[test]
    fn test_file_loader_read_errors_propagate() {
        use super::{FileLoaderError, Readable};

        let temp = assert_fs::TempDir::new().expect("Failed to create temp dir");
        let missing = temp.path().join("missing.txt");
        let io_error =
            std::fs::read_to_string(&missing).expect_err("missing file read must fail");

        // Reading a missing path fails for both flavors.
        let error = missing.clone().read();
        assert!(matches!(error, Err(FileLoaderError::IoError(_))));
        let error = missing.clone().read_with_path();
        assert!(matches!(error, Err(FileLoaderError::IoError(_))));

        // The `Result` wrappers forward errors from earlier pipeline stages.
        let error: Result<String, FileLoaderError> =
            Err::<std::path::PathBuf, _>(FileLoaderError::IoError(io_error)).read();
        assert!(matches!(error, Err(FileLoaderError::IoError(_))));
        let io_error =
            std::fs::read_to_string(&missing).expect_err("missing file read must fail");
        let error: Result<(std::path::PathBuf, String), FileLoaderError> =
            Err::<std::path::PathBuf, _>(FileLoaderError::IoError(io_error)).read_with_path();
        assert!(matches!(error, Err(FileLoaderError::IoError(_))));
    }

    #[test]
    fn test_file_loader_from_bytes_read_with_path_reports_memory_path() {
        let loader = FileLoader::from_bytes(b"raw-bytes".to_vec());
        let actual: Vec<(std::path::PathBuf, String)> = loader
            .read_with_path()
            .ignore_errors()
            .into_iter()
            .collect();

        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].0, std::path::PathBuf::from("<memory>"));
        assert_eq!(actual[0].1, "raw-bytes");
    }

    #[test]
    fn test_file_loader_from_bytes_invalid_utf8_errors() {
        let loader = FileLoader::from_bytes(vec![0xff, 0xfe, 0xfd]);
        let results: Vec<Result<String, super::FileLoaderError>> =
            loader.read().into_iter().collect();

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0].as_ref().err(),
            Some(super::FileLoaderError::StringUtf8Error(_))
        ));
    }
}
