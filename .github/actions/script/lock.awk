# Cargo's generated lockfile subset, not a TOML parser. Unknown fields, multiline/escaped
# strings and unsupported lock versions fail closed. No closure bytes are printed until
# every reachable identity and source has been checked. locals receives name<TAB>version.
function fail(why) {
  print "key.sh: " FILENAME ":" FNR ": " why > "/dev/stderr"
  failed = 1
  exit 1
}
function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s); return s }
function quoted(s) {
  if (s !~ /^"[^"\\]*"$/) fail("unsupported generated-lock string: " s)
  return substr(s, 2, length(s) - 2)
}
function finish(   identity) {
  if (!n) return
  if (in_deps) fail("unterminated dependencies array")
  if (name[n] == "" || version[n] == "") fail("package needs name and version")
  identity = kind[n] SUBSEP name[n] SUBSEP version[n] SUBSEP source[n]
  if (identity in identities) fail("duplicate package identity: " name[n] " " version[n] " " source[n])
  identities[identity] = n
  if (kind[n] == "package") byname[name[n]] = byname[name[n]] " " n
}
function resolve(dep,   words, count, want_source, candidates, size, k, i, matches, answer) {
  count = split(dep, words, " ")
  if (count < 1 || count > 3 || words[1] !~ /^[A-Za-z0-9_-]+$/)
    fail("unsupported dependency identity: " dep)
  if (count > 1 && words[2] !~ /^[0-9][A-Za-z0-9.+_-]*$/)
    fail("unsupported dependency version: " dep)
  if (count == 3) {
    if (words[3] !~ /^\([^() \t]+\)$/) fail("unsupported dependency source: " dep)
    want_source = substr(words[3], 2, length(words[3]) - 2)
  }
  size = split(byname[words[1]], candidates, " ")
  matches = 0
  for (k = 1; k <= size; k++) {
    i = candidates[k]
    if (count > 1 && version[i] != words[2]) continue
    if (count == 3 && source[i] != want_source) continue
    matches++
    answer = i
  }
  if (matches != 1) fail((matches ? "ambiguous" : "missing") " dependency identity: " dep)
  return answer
}
{
  line = trim($0)
  if (line == "" || line ~ /^#/) next
  if (line == "[[package]]" || line == "[[patch.unused]]") {
    if (!lock_version) fail("lock version must precede packages")
    finish()
    n++
    kind[n] = line == "[[package]]" ? "package" : "patch.unused"
    block[n] = line "\n"
    next
  }
  if (!n) {
    if (lock_version || line !~ /^version = [34]$/) fail("unsupported generated lock header: " line)
    lock_version = substr(line, 11)
    next
  }
  block[n] = block[n] $0 "\n"
  if (in_deps) {
    if (line == "]") { in_deps = 0; next }
    if (line !~ /^"[^"\\]+",$/) fail("unsupported generated dependency row: " line)
    dep = quoted(substr(line, 1, length(line) - 1))
    if (duplicate_dep[n SUBSEP dep]++) fail("duplicate dependency: " dep)
    dependencies[n, ++dep_count[n]] = dep
    next
  }
  if (line == "dependencies = [" || line == "dependencies = []") {
    if (field[n, "dependencies"]++) fail("duplicate dependencies field")
    in_deps = line == "dependencies = ["
    next
  }
  equal = index(line, " = ")
  if (!equal) fail("unsupported generated lock field: " line)
  key = substr(line, 1, equal - 1)
  value = quoted(substr(line, equal + 3))
  if (field[n, key]++) fail("duplicate package field: " key)
  if (key == "name") {
    if (value !~ /^[A-Za-z0-9_-]+$/) fail("unsupported package name: " value)
    name[n] = value
  } else if (key == "version") {
    if (value !~ /^[0-9][A-Za-z0-9.+_-]*$/) fail("unsupported package version: " value)
    version[n] = value
  } else if (key == "source") {
    if (value == "" || value ~ /[ \t()]/) fail("unsupported package source: " value)
    source[n] = value
  } else if (key == "checksum") {
    if (length(value) != 64 || value !~ /^[0-9a-f]+$/) fail("unsupported package checksum: " value)
    checksum[n] = value
  } else fail("unsupported generated lock field: " key)
}
END {
  if (failed) exit 1
  finish()
  if (!n) fail("no packages in lockfile")
  root = resolve("scripts")
  if (source[root] != "") fail("scripts is not a local package")
  queue[1] = root
  tail = 1
  for (head = 1; head <= tail; head++) {
    i = queue[head]
    if (seen[i]++) continue
    if (source[i] == "") {
      if (checksum[i] != "") fail("local package has a checksum: " name[i])
    } else if (source[i] !~ /^registry\+/ || checksum[i] == "") {
      fail("unhashable dependency " name[i] " " version[i] ": needs a registry checksum or checked-out local source")
    }
    for (j = 1; j <= dep_count[i]; j++) queue[++tail] = resolve(dependencies[i, j])
  }
  # Commit output only after the entire reachable graph has validated.
  printf "lock-version %s\n", lock_version
  for (i = 1; i <= n; i++) if (seen[i]) {
    printf "%s", block[i]
    if (source[i] == "") print name[i] "\t" version[i] > locals
  }
  close(locals)
}
