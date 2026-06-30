# derive the .wtd dir from this Makefile's own location (portable — no hardcoded home)
WTD := $(patsubst %/,%,$(dir $(realpath $(lastword $(MAKEFILE_LIST)))))/.wtd

.PHONY: install agent add-repo review repos tokens
install:
	@$(WTD)/scripts/install.sh

# make agent repo=<slug> name=<name> [refs="<slug> <slug>"]
agent:
	@$(WTD)/scripts/agent.sh "$(repo)" "$(name)" $(refs)

# make add-repo slug=<slug> url=<git-url>
add-repo:
	@$(WTD)/scripts/add-repo.sh "$(slug)" "$(url)"

# make review repo=<slug> name=<name> [ARGS="-i --base origin/dev"]
review:
	@$(WTD)/scripts/review.sh "$(repo)" "$(name)" $(ARGS)

repos:
	@grep -v '^#' $(WTD)/repos.tsv 2>/dev/null || echo "(no repos registered)"

tokens:
	@$(WTD)/scripts/tokens.sh $(ARGS)
