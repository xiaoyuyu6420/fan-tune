export function Name() { return "Maxsun ENE_Chip 6k7750"; }
export function VendorId() { return 0x0CF2; }
export function ProductId() { return [0x7750, 0x7758]; }
export function Documentation() { return "1,1"; }
export function Size() { return [1, 1]; }
export function DeviceType() { return "lightingcontroller"; }
export function Type() { return "rawusb"; }

import systeminfo from "@SignalRGB/systeminfo";

//插件作者:Sky QQ: 3814725166
//gitlab: https://gitlab.com/nollie/chinese-motherboard-signalrgb-plugin
//因为不是所有铭瑄主板都有单独适配，如果使用Default Maxsun device出现问题，作者不承担责任
export function ControllableParameters() {
	return [
		{ "property": "shutdownColor", "label": "Shutdown Color", "min": "0", "max": "360", "type": "color", "default": "000000" },
		{ "property": "LightingMode", "label": "Lighting Mode", "type": "combobox", "values": ["Canvas", "Forced"], "default": "Canvas" },
		{ "property": "forcedColor", "label": "Forced Color", "min": "0", "max": "360", "type": "color", "default": "000000" },
		// {"property":"Slider-1", "label":"Slider_name","step":"50", "type":"number","min":"200", "max":"18000","default":"800"},//数字滑块
		// {"property":"BOOL-1", "label":"BOOL_name", "type":"boolean", "default":"0"},//布尔值/开关
		// {"property":"Color-1", "label":"Color_name","min":"0","max":"360","type":"color","default":"#000000"},//RGB颜色选择器
	];
}

const Channel_1 = {
	device_name: "Channel 1",
	device_type: "ARGBChannel",
	leds_const: 128,
	// device_type: "RGBSubdevice",
	address: 0xe300,
	commit: 0xe02f,
	mode: 0xe021,
	speed: 0xe022,
	direction: 0xe023,
	brightness: 0xe029,
	count: 0,
};

const Channel_2 = {
	device_name: "Channel 2",
	device_type: "ARGBChannel",
	leds_const: 128,
	// device_type: "RGBSubdevice",
	address: 0xe480,
	commit: 0xe03f,
	mode: 0xe031,
	speed: 0xe032,
	direction: 0xe033,
	brightness: 0xe039,
	count: 0,
};

const RGB_12v_1 = {
	device_name: "12v RGB 1",
	device_type: "RGBSubdevice",
	leds_const: 1,
	// device_type: "RGBSubdevice",
	address: 0xe600,
	commit: 0xe04f,
	mode: 0xe041,
	speed: 0xe042,
	direction: 0xe043,
	brightness: 0xe049,
	count: 0,
};

const RGB_12v_2 = {
	device_name: "12v RGB 2",
	device_type: "RGBSubdevice",
	leds_const: 1,
	address: 0xe603,
	commit: 0xe05f,
	mode: 0xe051,
	speed: 0xe052,
	direction: 0xe053,
	brightness: 0xe059,
	count: 0,
};

const z790white_left_Back_RGB = {
	device_name: "Left Back RGB",
	device_type: "RGBSubdevice",
	leds_const: 6,
	// device_type: "RGBSubdevice",
	address: 0xe606,
	commit: 0xe06f,
	mode: 0xe061,
	speed: 0xe062,
	direction: 0xe063,
	brightness: 0xe069,
	count: 0,
};


//这是核心识别函数，在这里添加设备
let Motherboard;
const MOTHERBOARDS = {
	"MS-iCraft Z790 White": {
		layout: [Channel_1, Channel_2, RGB_12v_1, RGB_12v_2, z790white_left_Back_RGB]
	},
	"Default Maxsun device": {
		layout: [Channel_1, Channel_2, RGB_12v_1, RGB_12v_2]
	},
};

export function Initialize() {
	const motherboardInfo = systeminfo.GetMotherboardInfo();
	const MOTHERBOARD_NAME = motherboardInfo.model;
	const MOTHERBOARD_BRAND = motherboardInfo.manufacturer;

	if (MOTHERBOARD_BRAND != "Maxsun") {
		device.log("Maxsun ENE_Chip 6k7750 plugin: No Maxsun device detected");
		return;
	}
	device.log(motherboardInfo);
	device.log(`Maxsun ENE_Chip 6k7750 plugins MotherboardName: ${MOTHERBOARD_NAME}`);
	device.log(`Maxsun ENE_Chip 6k7750 plugins MotherboardBrand: ${MOTHERBOARD_BRAND}`);

	Motherboard = MOTHERBOARDS[MOTHERBOARD_NAME]
	device.setName(`${MOTHERBOARD_NAME}`);
	if (!Motherboard) {
		Motherboard = MOTHERBOARDS["Default Maxsun device"]
		device.setName("Default Maxsun Device");
		device.log(`No matching configuration found for "${MOTHERBOARD_NAME}", using default Maxsun device configuration`);
		device.log(`没有找到"${MOTHERBOARD_NAME}的配置",当前加载为 (默认Maxsun设备配置)`);
	}

	setupDevice();
	// device.setFrameRateTarget(60);
	// 这个函数本来可以提升FPS帧率，不过实测帧率超过20就会卡顿，刷新更慢
}

export function setupDevice() {
	let leds_num = Motherboard.layout.length;
	for (let idx = 0; idx < leds_num; idx++) {
		switch (Motherboard.layout[idx].device_type) {
			case "ARGBChannel":
				//通道设备
				device.addChannel(Motherboard.layout[idx].device_name, Motherboard.layout[idx].leds_const);
				device.channel(Motherboard.layout[idx].device_name).SetLedLimit(Motherboard.layout[idx].leds_const);
				break;
			case "RGBSubdevice":
				//子设备
				device.createSubdevice(Motherboard.layout[idx].device_name);
				device.setSubdeviceName(Motherboard.layout[idx].device_name, Motherboard.layout[idx].device_name);//设置子设备名称
				device.setSubdeviceSize(Motherboard.layout[idx].device_name, Motherboard.layout[idx].leds_const, 1);
				let SubdeviceLEDPositions = [];
				for (let jdx = 0; jdx < Motherboard.layout[idx].leds_const; jdx++) {
					SubdeviceLEDPositions.push([jdx, 0]);
				}
				device.setSubdeviceLeds(Motherboard.layout[idx].device_name, Motherboard.layout[idx].device_name, SubdeviceLEDPositions);
				//device.setSubdeviceImageUrl("设备名称,字符串", "图片地址空着就行");
				break;
		}
	}
}

export function Render() {
	//灯光控制包
	let dev_num = Motherboard.layout.length;

	readCommitLongPacket();

	for (let idx = 0; idx < dev_num; idx++) {
		//读取8字节控制包，里面的数据其实没用c0 81 0000 e0a2 0008
		readCommitPacket();
		SendSingleDevice(idx);
		//根据主板配置往寄存器写入数据
		sendControlCommit(idx);
		//4字节控制包
		sendCommitPacket();
	}

	device.pause(12);
	//如果卡帧,把pause里面的1提高,每次+1,直到不卡
}

export function sendControlCommit(idx) {
	//开发者并不知道这几包有什么用，但是发就对了
	//					地址					   数据  数据长度	
	sendControlPacket(Motherboard.layout[idx].mode, [0x01], 1);
	// device.pause(1);
	sendControlPacket(Motherboard.layout[idx].speed, [0x00], 1);
	// device.pause(1);
	sendControlPacket(Motherboard.layout[idx].direction, [0x00], 1);
	// device.pause(1);
	sendControlPacket(Motherboard.layout[idx].brightness, [0x00], 1);
	// device.pause(1);
	sendControlPacket(Motherboard.layout[idx].commit, [0x01], 1);
	// device.pause(1);
}

export function Shutdown() {
	let dev_num = Motherboard.layout.length;

	readCommitLongPacket();

	for (let idx = 0; idx < dev_num; idx++) {
		readCommitPacket();
		let packet = new Array(Motherboard.layout[idx].leds_const * 3);
		let RGB_Data = [];

		let Shutdown_Color = device.createColorArray(shutdownColor, 1, "Inline", "RGB");
		let leds_num = Motherboard.layout[idx].leds_const;
		for (let idx = 0; idx < leds_num; idx++) {
			let iIdx = idx * 3;
			packet[iIdx] = Shutdown_Color[0];
			packet[iIdx + 1] = Shutdown_Color[1];
			packet[iIdx + 2] = Shutdown_Color[2];
		}

		if (packet.length > 0) {
			sendControlPacket(Motherboard.layout[idx].address, packet, packet.length);
		}
		//根据主板配置往寄存器写入数据
		sendControlCommit(idx);
		//4字节控制包
		sendCommitPacket();
	}
}

export function ColorTohex(hex) {
	let r = parseInt(hex.slice(1, 3), 16);
	let g = parseInt(hex.slice(3, 5), 16);
	let b = parseInt(hex.slice(5, 7), 16);
	let color = [];
	color[0] = r;
	color[1] = g;
	color[2] = b;
	return color;
}

function SendSingleDevice(idx) {
	let packet = new Array(Motherboard.layout[idx].leds_const * 3);
	let RGB_Data = [];
	//判断是通道还是子设备,获取到颜色
	switch (Motherboard.layout[idx].device_type) {
		case "ARGBChannel":
			RGB_Data = getChannelColor(idx);
			break;
		case "RGBSubdevice":
			RGB_Data = getSubdeviceColor(idx);
			break;
	}

	//裁剪数据
	let leds_num = Motherboard.layout[idx].leds_const;

	for (let idx = 0; idx < leds_num; idx++) {
		let iIdx = idx * 3;
		packet[iIdx] = RGB_Data[iIdx];
		packet[iIdx + 1] = RGB_Data[iIdx + 1];
		packet[iIdx + 2] = RGB_Data[iIdx + 2];
	}

	if (packet.length > 0) {
		//发送数据包
		sendControlPacket(Motherboard.layout[idx].address, packet, packet.length);
	}
}

function getChannelColor(idx) {
	let RGB_Data = device.channel(Motherboard.layout[idx].device_name).getColors("Inline", "RGB");
	return RGB_Data;
}

function getSubdeviceColor(idx) {
	let Leds_num = Motherboard.layout[idx].leds_const;
	let RGB_Data = [];
	for (let jdx = 0; jdx < Leds_num; jdx++) {
		let color = device.subdeviceColor(Motherboard.layout[idx].device_name, jdx, 0)
		RGB_Data.push(...color);
	}
	return RGB_Data;
}

export function Validate(endpoint) {
	return endpoint.interface === 1 && endpoint.usage === 0x00a1 && endpoint.usage_page === 0xff72 && endpoint.collection === 0x0000;
}

export function ImageUrl() {
	return "https://gitlab.com/nollie/chinese-motherboard-signalrgb-plugin/-/raw/main/image/maxsun_motherboard.png";
}

export function readCommitLongPacket() {
	let Address = [0xb500]
	let Data = [];
	let Len = 5;

	readControlPacket(Address[0], Data, Len);

	Address = [0x4000, 0x4001, 0x0244, 0xe0a1, 0xe7a1]
	Len = 1;
	for (let idx = 0; idx < 5; idx++) {
		readControlPacket(Address[idx], Data, Len);
	}

	Address = [0xe0a2, 0xe0a4, 0xe0a6, 0xe0a8, 0xe0aa]
	Len = 2;
	for (let idx = 0; idx < 5; idx++) {
		readControlPacket(Address[idx], Data, Len);
	}
}

export function readCommitPacket() {
	const Address = 0xe0a2;
	// const Data = 0x01;
	let Data = [];
	const Len = 8;
	return readControlPacket(Address, Data, Len);
}

export function sendCommitPacket() {
	let Address = [0xe021, 0xe02f, 0xe031, 0xe03f];
	const Len = 1;
	for (let idx = 0; idx < 4; idx++) {
		sendControlPacket(Address[idx], [0x01], Len);
	}
}

export function readControlPacket(index, data, length) {
	//                  iType, iRequest, iValue, iReqIdx, pBuf, iLen, iTimeout
	return device.control_transfer(0xC0, 0x81, 0, index, data, length, 1000);
}

export function sendControlPacket(index, data, length) {
	//                  iType, iRequest, iValue, iReqIdx, pBuf, iLen, iTimeout
	device.control_transfer(0x40, 0x80, 0, index, data, length, 1000);
	//device.pause(1)
}
