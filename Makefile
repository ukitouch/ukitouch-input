.SILENT:

.PHONY: dist clean

NAME      = ukitouch-input
OUT       = dist
MANIFEST  = manifest.json

# Platform-specific tools
ifeq ($(OS),Windows_NT)
	TARGET   = target/release/$(NAME).exe
	MKDIR    = powershell -NoProfile -Command "New-Item -ItemType Directory -Force"
	CP       = powershell -NoProfile -Command "Copy-Item -Force"
	RM       = powershell -NoProfile -Command "Remove-Item -Recurse -Force -ErrorAction SilentlyContinue"
	ZIP      = powershell -NoProfile -Command "Compress-Archive -Force"
else
	TARGET   = target/release/$(NAME)
MKDIR    = mkdir -p
	CP       = cp
	RM       = rm -rf
	ZIP      = cd $(OUT) && zip -r
endif

build:
	cargo build --release
	-$(MKDIR) $(OUT)
	$(CP) $(TARGET) $(OUT)/
	$(CP) $(MANIFEST) $(OUT)/
ifeq ($(OS),Windows_NT)
	$(ZIP) -Path '$(OUT)\$(NAME).exe,$(OUT)\$(MANIFEST)' -DestinationPath '$(OUT)\$(NAME).zip'
else
	$(ZIP) $(NAME).zip $(NAME) $(MANIFEST)
endif

clean:
	-$(RM) $(OUT)
	cargo clean