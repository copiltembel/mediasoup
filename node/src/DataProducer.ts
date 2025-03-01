import { Logger } from './Logger';
import { EnhancedEventEmitter } from './EnhancedEventEmitter';
import { Channel } from './Channel';
import { TransportInternal } from './Transport';
import { parseSctpStreamParameters, SctpStreamParameters } from './SctpParameters';
import { AppData } from './types';
import * as FbsTransport from './fbs/transport';
import * as FbsNotification from './fbs/notification';
import * as FbsRequest from './fbs/request';
import * as FbsDataProducer from './fbs/data-producer';

export type DataProducerOptions<DataProducerAppData extends AppData = AppData> =
{
	/**
	 * DataProducer id (just for Router.pipeToRouter() method).
	 */
	id?: string;

	/**
	 * SCTP parameters defining how the endpoint is sending the data.
	 * Just if messages are sent over SCTP.
	 */
	sctpStreamParameters?: SctpStreamParameters;

	/**
	 * A label which can be used to distinguish this DataChannel from others.
	 */
	label?: string;

	/**
	 * Name of the sub-protocol used by this DataChannel.
	 */
	protocol?: string;

	/**
	 * Whether the data producer must start in paused mode. Default false.
	 */
	paused?: boolean;

	/**
	 * Custom application data.
	 */
	appData?: DataProducerAppData;
};

export type DataProducerStat =
{
	type: string;
	timestamp: number;
	label: string;
	protocol: string;
	messagesReceived: number;
	bytesReceived: number;
};

/**
 * DataProducer type.
 */
export type DataProducerType = 'sctp' | 'direct';

export type DataProducerEvents =
{
	transportclose: [];
	// Private events.
	'@close': [];
};

export type DataProducerObserverEvents =
{
	close: [];
	pause: [];
	resume: [];
};

type DataProducerDump = DataProducerData &
{
	id: string;
	paused: boolean;
};

type DataProducerInternal = TransportInternal &
{
	dataProducerId: string;
};

type DataProducerData =
{
	type: DataProducerType;
	sctpStreamParameters?: SctpStreamParameters;
	label: string;
	protocol: string;
};

const logger = new Logger('DataProducer');

export class DataProducer<DataProducerAppData extends AppData = AppData>
	extends EnhancedEventEmitter<DataProducerEvents>
{
	// Internal data.
	readonly #internal: DataProducerInternal;

	// DataProducer data.
	readonly #data: DataProducerData;

	// Channel instance.
	readonly #channel: Channel;

	// Closed flag.
	#closed = false;

	// Paused flag.
	#paused = false;

	// Custom app data.
	#appData: DataProducerAppData;

	// Observer instance.
	readonly #observer = new EnhancedEventEmitter<DataProducerObserverEvents>();

	/**
	 * @private
	 */
	constructor(
		{
			internal,
			data,
			channel,
			paused,
			appData
		}:
		{
			internal: DataProducerInternal;
			data: DataProducerData;
			channel: Channel;
			paused: boolean;
			appData?: DataProducerAppData;
		}
	)
	{
		super();

		logger.debug('constructor()');

		this.#internal = internal;
		this.#data = data;
		this.#channel = channel;
		this.#paused = paused;
		this.#appData = appData || {} as DataProducerAppData;

		this.handleWorkerNotifications();
	}

	/**
	 * DataProducer id.
	 */
	get id(): string
	{
		return this.#internal.dataProducerId;
	}

	/**
	 * Whether the DataProducer is closed.
	 */
	get closed(): boolean
	{
		return this.#closed;
	}

	/**
	 * DataProducer type.
	 */
	get type(): DataProducerType
	{
		return this.#data.type;
	}

	/**
	 * SCTP stream parameters.
	 */
	get sctpStreamParameters(): SctpStreamParameters | undefined
	{
		return this.#data.sctpStreamParameters;
	}

	/**
	 * DataChannel label.
	 */
	get label(): string
	{
		return this.#data.label;
	}

	/**
	 * DataChannel protocol.
	 */
	get protocol(): string
	{
		return this.#data.protocol;
	}

	/**
	 * Whether the DataProducer is paused.
	 */
	get paused(): boolean
	{
		return this.#paused;
	}

	/**
	 * App custom data.
	 */
	get appData(): DataProducerAppData
	{
		return this.#appData;
	}

	/**
	 * App custom data setter.
	 */
	set appData(appData: DataProducerAppData)
	{
		this.#appData = appData;
	}

	/**
	 * Observer.
	 */
	get observer(): EnhancedEventEmitter<DataProducerObserverEvents>
	{
		return this.#observer;
	}

	/**
	 * Close the DataProducer.
	 */
	close(): void
	{
		if (this.#closed)
		{
			return;
		}

		logger.debug('close()');

		this.#closed = true;

		// Remove notification subscriptions.
		this.#channel.removeAllListeners(this.#internal.dataProducerId);

		/* Build Request. */
		const requestOffset = new FbsTransport.CloseDataProducerRequestT(
			this.#internal.dataProducerId
		).pack(this.#channel.bufferBuilder);

		this.#channel.request(
			FbsRequest.Method.TRANSPORT_CLOSE_DATAPRODUCER,
			FbsRequest.Body.Transport_CloseDataProducerRequest,
			requestOffset,
			this.#internal.transportId
		).catch(() => {});

		this.emit('@close');

		// Emit observer event.
		this.#observer.safeEmit('close');
	}

	/**
	 * Transport was closed.
	 *
	 * @private
	 */
	transportClosed(): void
	{
		if (this.#closed)
		{
			return;
		}

		logger.debug('transportClosed()');

		this.#closed = true;

		// Remove notification subscriptions.
		this.#channel.removeAllListeners(this.#internal.dataProducerId);

		this.safeEmit('transportclose');

		// Emit observer event.
		this.#observer.safeEmit('close');
	}

	/**
	 * Dump DataProducer.
	 */
	async dump(): Promise<DataProducerDump>
	{
		logger.debug('dump()');

		const response = await this.#channel.request(
			FbsRequest.Method.DATAPRODUCER_DUMP,
			undefined,
			undefined,
			this.#internal.dataProducerId
		);

		/* Decode Response. */
		const produceResponse = new FbsDataProducer.DumpResponse();

		response.body(produceResponse);

		return parseDataProducerDumpResponse(produceResponse);
	}

	/**
	 * Get DataProducer stats.
	 */
	async getStats(): Promise<DataProducerStat[]>
	{
		logger.debug('getStats()');

		const response = await this.#channel.request(
			FbsRequest.Method.DATAPRODUCER_GET_STATS,
			undefined,
			undefined,
			this.#internal.dataProducerId
		);

		/* Decode Response. */
		const data = new FbsDataProducer.GetStatsResponse();

		response.body(data);

		return [ parseDataProducerStats(data) ];
	}

	/**
	 * Pause the DataProducer.
	 */
	async pause(): Promise<void>
	{
		logger.debug('pause()');

		const wasPaused = this.#paused;

		await this.#channel.request(
			FbsRequest.Method.DATAPRODUCER_PAUSE,
			undefined,
			undefined,
			this.#internal.dataProducerId
		);

		this.#paused = true;

		// Emit observer event.
		if (!wasPaused)
		{
			this.#observer.safeEmit('pause');
		}
	}

	/**
	 * Resume the DataProducer.
	 */
	async resume(): Promise<void>
	{
		logger.debug('resume()');

		const wasPaused = this.#paused;

		await this.#channel.request(
			FbsRequest.Method.DATAPRODUCER_RESUME,
			undefined,
			undefined,
			this.#internal.dataProducerId
		);

		this.#paused = false;

		// Emit observer event.
		if (wasPaused)
		{
			this.#observer.safeEmit('resume');
		}
	}

	/**
	 * Send data (just valid for DataProducers created on a DirectTransport).
	 */
	send(
		message: string | Buffer,
		ppid?: number,
		subchannels?: number[],
		requiredSubchannel?: number
	): void
	{
		if (typeof message !== 'string' && !Buffer.isBuffer(message))
		{
			throw new TypeError('message must be a string or a Buffer');
		}

		/*
		 * +-------------------------------+----------+
		 * | Value                         | SCTP     |
		 * |                               | PPID     |
		 * +-------------------------------+----------+
		 * | WebRTC String                 | 51       |
		 * | WebRTC Binary Partial         | 52       |
		 * | (Deprecated)                  |          |
		 * | WebRTC Binary                 | 53       |
		 * | WebRTC String Partial         | 54       |
		 * | (Deprecated)                  |          |
		 * | WebRTC String Empty           | 56       |
		 * | WebRTC Binary Empty           | 57       |
		 * +-------------------------------+----------+
		 */

		if (typeof ppid !== 'number')
		{
			ppid = (typeof message === 'string')
				? message.length > 0 ? 51 : 56
				: message.length > 0 ? 53 : 57;
		}

		// Ensure we honor PPIDs.
		if (ppid === 56)
		{
			message = ' ';
		}
		else if (ppid === 57)
		{
			message = Buffer.alloc(1);
		}

		const builder = this.#channel.bufferBuilder;

		let dataOffset = 0;

		const subchannelsOffset = FbsDataProducer.SendNotification.createSubchannelsVector(
			builder, subchannels ?? []
		);

		if (typeof message === 'string')
		{
			message = Buffer.from(message);
		}

		dataOffset = FbsDataProducer.SendNotification.createDataVector(builder, message);

		const notificationOffset = FbsDataProducer.SendNotification.createSendNotification(
			builder,
			ppid,
			dataOffset,
			subchannelsOffset,
			requiredSubchannel ?? null
		);

		this.#channel.notify(
			FbsNotification.Event.DATAPRODUCER_SEND,
			FbsNotification.Body.DataProducer_SendNotification,
			notificationOffset,
			this.#internal.dataProducerId
		);
	}

	private handleWorkerNotifications(): void
	{
		// No need to subscribe to any event.
	}
}

export function dataProducerTypeToFbs(type: DataProducerType): FbsDataProducer.Type
{
	switch (type)
	{
		case 'sctp':
			return FbsDataProducer.Type.SCTP;
		case 'direct':
			return FbsDataProducer.Type.DIRECT;
		default:
			throw new TypeError('invalid DataConsumerType: ${type}');
	}
}

export function dataProducerTypeFromFbs(type: FbsDataProducer.Type): DataProducerType
{
	switch (type)
	{
		case FbsDataProducer.Type.SCTP:
			return 'sctp';
		case FbsDataProducer.Type.DIRECT:
			return 'direct';
	}
}

export function parseDataProducerDumpResponse(
	data: FbsDataProducer.DumpResponse
): DataProducerDump
{
	return {
		id                   : data.id()!,
		type                 : dataProducerTypeFromFbs(data.type()),
		sctpStreamParameters : data.sctpStreamParameters() !== null ?
			parseSctpStreamParameters(data.sctpStreamParameters()!) :
			undefined,
		label    : data.label()!,
		protocol : data.protocol()!,
		paused   : data.paused()
	};
}

function parseDataProducerStats(
	binary: FbsDataProducer.GetStatsResponse
): DataProducerStat
{
	return {
		type             : 'data-producer',
		timestamp        : Number(binary.timestamp()),
		label            : binary.label()!,
		protocol         : binary.protocol()!,
		messagesReceived : Number(binary.messagesReceived()),
		bytesReceived    : Number(binary.bytesReceived())
	};
}
