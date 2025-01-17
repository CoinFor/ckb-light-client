CARGO_TARGET_DIR ?= target
COVERAGE_PROFRAW_DIR ?= ${CARGO_TARGET_DIR}/coverage
GRCOV_OUTPUT ?= coverage-report.info
GRCOV_EXCL_START = ^\s*((log::)?(trace|debug|info|warn|error)|(debug_)?assert(_eq|_ne|_error_eq))!\($$
GRCOV_EXCL_STOP  = ^\s*\)(;)?$$
GRCOV_EXCL_LINE = \s*((log::)?(trace|debug|info|warn|error)|(debug_)?assert(_eq|_ne|_error_eq))!\(.*\)(;)?$$

coverage-clean:
	rm -rf "${CARGO_TARGET_DIR}/*.profraw" "${GRCOV_OUTPUT}" "${GRCOV_OUTPUT:.info=}"

coverage-install-tools:
	rustup component add llvm-tools-preview
	grcov --version || cargo install grcov

coverage-run-unittests:
	mkdir -p "${COVERAGE_PROFRAW_DIR}"
	rm -f "${COVERAGE_PROFRAW_DIR}/*.profraw"
	RUSTFLAGS="${RUSTFLAGS} -Cinstrument-coverage" \
		LLVM_PROFILE_FILE="${COVERAGE_PROFRAW_DIR}/unittests-%p-%m.profraw" \
			cargo test --all

coverage-collect-data:
	grcov "${COVERAGE_PROFRAW_DIR}" --binary-path "${CARGO_TARGET_DIR}/debug/" \
		-s . -t lcov --branch --ignore-not-existing \
		--ignore "/*" \
		--ignore "*/tests/*" \
		--ignore "*/tests.rs" \
		--excl-br-start "${GRCOV_EXCL_START}" --excl-br-stop "${GRCOV_EXCL_STOP}" \
		--excl-start    "${GRCOV_EXCL_START}" --excl-stop    "${GRCOV_EXCL_STOP}" \
		--excl-br-line  "${GRCOV_EXCL_LINE}" \
		--excl-line     "${GRCOV_EXCL_LINE}" \
		-o "${GRCOV_OUTPUT}"

coverage-generate-report:
	genhtml -o "${GRCOV_OUTPUT:.info=}" "${GRCOV_OUTPUT}"
