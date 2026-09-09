package cmd

import (
	"bytes"
	"runtime"
	"strings"
	"testing"
)

func TestVersionCommand(t *testing.T) {
	var out, errOut bytes.Buffer

	root := NewRootCommand()
	root.SetOut(&out)
	root.SetErr(&errOut)
	root.SetArgs([]string{"version"})

	if err := root.Execute(); err != nil {
		t.Fatalf("execute version: %v", err)
	}

	got := out.String()
	want := "kist-go " + Version() + " " + runtime.GOOS + "/" + runtime.GOARCH + " " + runtime.Version() + "\n"
	if got != want {
		t.Errorf("stdout = %q, want %q", got, want)
	}
	if errOut.Len() != 0 {
		t.Errorf("stderr = %q, want empty", errOut.String())
	}
}

func TestVersionCommandRejectsArgs(t *testing.T) {
	var out, errOut bytes.Buffer

	root := NewRootCommand()
	root.SetOut(&out)
	root.SetErr(&errOut)
	root.SetArgs([]string{"version", "extra"})

	err := root.Execute()
	if err == nil {
		t.Fatal("execute version with an argument: got nil error, want failure")
	}
	if !strings.Contains(err.Error(), "unknown command") && !strings.Contains(err.Error(), "arg") {
		t.Errorf("error = %q, want it to mention the unexpected argument", err)
	}
}

func TestRootWithoutArgsShowsHelp(t *testing.T) {
	var out, errOut bytes.Buffer

	root := NewRootCommand()
	root.SetOut(&out)
	root.SetErr(&errOut)
	root.SetArgs(nil)

	if err := root.Execute(); err != nil {
		t.Fatalf("execute root: %v", err)
	}
	if !strings.Contains(out.String(), "kist-go") {
		t.Errorf("stdout = %q, want the help text", out.String())
	}
}
