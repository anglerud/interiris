.DUMMY: debug release

debug:
	cargo build
	sudo setcap 'cap_net_raw=ep' target/debug/internet_quality_monitor

release:
	cargo build --release
	sudo setcap 'cap_net_raw=ep' target/release/internet_quality_monitor

docker:
	docker build -t interiris:latest .
