using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Runtime.Serialization;
using System.Threading.Tasks;
using Coflnet.Sky.Commands.MC;
using Coflnet.Sky.Commands.Shared;
using Microsoft.AspNetCore.DataProtection;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.Logging;
using Newtonsoft.Json;
using WebSocketSharp;
using WebSocketSharp.Server;

namespace Coflnet.Sky.ModCommands.Services.Vps;

public class VpsSocket : WebSocketBehavior
{
    public string IP { get; set; }
    private ILogger<VpsSocket> logger;

    protected override void OnOpen()
    {
        logger = DiHandler.GetService<ILogger<VpsSocket>>();
        var args = QueryString;
        IP = args["ip"];
        var secret = args["secret"];
        var configuration = DiHandler.GetService<IConfiguration>();
        if (secret != configuration["vps:secret"] && configuration["vps:secret"] != null)
        {
            Close();
            return;
        }
        var vpsService = DiHandler.GetService<VpsInstanceManager>();
        vpsService.OnInstanceCreated += Distributeupdate;
        Task.Run(async () =>
        {
            try
            {
                var vps = await vpsService.GetRunningVps(IP);
                Send(JsonConvert.SerializeObject(Response.Create("init", vps)));
                vpsService.Connected(IP);
            }
            catch (System.Exception e)
            {
                logger.LogError(e, "Boostraping VPS list");
            }
            while (this.ReadyState == WebSocketState.Open)
            {
                await Task.Delay(60_000);
                vpsService.Connected(IP);
            }
            logger.LogWarning("VPS {ip} disconnected", IP);
            vpsService.OnInstanceCreated -= Distributeupdate;
        });

    }

    private void Distributeupdate(VPsStateUpdate update)
    {
        try
        {
            var isForThisconnection = update.Instance.HostMachineIp == IP || ShouldDistributeOffState(update);
            logger.LogInformation("Received update {ip} for {target} ({forThis}) {id}", IP, update.Instance.HostMachineIp, isForThisconnection, update.Instance.Id);
            if (isForThisconnection)
            {
                Send(JsonConvert.SerializeObject(Response.Create("configUpdate", update)));
                logger.LogInformation("Sent update {ip} for {target} {id}", IP, update.Instance.HostMachineIp, update.Instance.Id);
            }
        }
        catch (Exception e)
        {
            logger.LogError(e, "Distributing VPS update");
        }

        static bool ShouldDistributeOffState(VPsStateUpdate update)
        {
            return update.Instance.Context.ContainsKey("turnedOff");
        }
    }

    protected override void OnMessage(MessageEventArgs e)
    {
        Task.Run(async () =>
        {
            var source = DiHandler.GetService<ActivitySource>();
            using var activity = source.StartActivity("VpsSocket.OnMessage");
            try
            {
                await RunCommand(e);
            }
            catch (Exception ex)
            {
                using var errorActivity = source.StartActivity("error");
                errorActivity.Log(ex.ToString());
                logger.LogError(ex, "Error in VpsSocket");
                Send(JsonConvert.SerializeObject(new { type = "error", data = ex.Message }));
            }
        });
    }

    private async Task RunCommand(MessageEventArgs e)
    {
        var response = JsonConvert.DeserializeObject<Response>(e.Data);
        Activity.Current.AddTag("type", response.type);
        Activity.Current.Log(response.data);
        if (response.type == "extraUpdate")
        {
            var extraConfig = JsonConvert.DeserializeObject<ExtraConfig>(response.data);
            await DiHandler.GetService<VpsInstanceManager>().PersistExtra(extraConfig.userId, extraConfig.extraConfig);
        }
        Console.WriteLine($"Received {response.type} from {IP}");
    }

    public class ExtraConfig
    {
        public string userId { get; set; }
        public string extraConfig { get; set; }
    }
}

public class TPM
{
    public static readonly string PlusDefault = """
    {

    //Put your minecraft IGN here. To use multiple, follow this format: ["account1", "account2"],
    "igns": [""],
    //Used in backend. Get it from the /get_discord_id command in TPM server
    "discordID": "",
    //Refer to https://discord.com/channels/1261825756615540836/1265035635845234792 for help
    "webhook": "",
    //{0} is item. {1} is profit. {2} is price. {3} is target. {4} is buyspeed. {5} is BED or NUGGET. {6} is finder. {7} is the auctionID. {8} is the shortened price. {9} is the bot's username
    "webhookFormat": "You bought [``{0}``](https:\\/\\/sky.coflnet.com\\/auction\\/{7}) for ``{2}`` (``{1}`` profit) in ``{4}ms``",
    //Send every flip seen to this webhook. Good for testing configs
    "sendAllFlips": "",
    //Flip on a friend's island
    "visitFriend": "",
    //Required to use relist!! Will flip in if you don't have a cookie
    "useCookie": true,
    //If cookie is under this time then buy a new one. Leave blank to never auto buy a cookie.
    //Use y, d, or h to set time
    "autoCookie": "1h",
    //Don't claim coop's auctions
    "angryCoopPrevention": false,
    //Automatically list auctions
    "relist": true,
    //Delay between actions. For example, opening flips
    "delay": 250,
    //Set up different list price ranges and their corresponding percent off of target price. (The lower value of the range is inclusive, the higher value is exclusive)
    "percentOfTarget": ["0", "10b", 97],
    //Amount of time (hours) to list an auction.
    //Works the same as percentOfTarget but for time auctions are listed!
    "listHours": ["0", "10b", 48],
    //Chooses how long it should wait between opening bin auction view and clicking on the nugget (used to manage cofl delay)
    //Time is in milliseonds
    //Format is the same as percentOfTarget and listHours but the bounds (aka first and second number) are for profit
    "delayTime": ["0", "10b", 0],
    //Delay between clicks for bed spam (ideally use 100-125)
    "clickDelay": 125,
    //Decides the way to buy beds
    "bedSpam": false,
    //Won't show spam messages
    "blockUselessMessages": true,
    //Digit to round relist price to. For example 6 would round 1,234,567 to 1,200,000
    "roundTo": 6,
    //Skip the confirm screen on NUGGET flips (50ms faster but higher ban rate)
    //This is an OR statement btw
    "skip": {

        //Skip on every flip
        "always": false,
        //Skip on flips with a profit over x
        "minProfit": "25m",
        //Skip on flips over this %
        "profitPercentage": "500",
        //Skip on flips over this price
        "minPrice": "500m",
        //Skip on user finder flips
        "userFinder": true,
        //Skip on cosmetic flips
        "skins": true

    },
    //Items that you don't want automatically listed
    "doNotRelist": {

        //Items over x profit
        "profitOver": "50m",
        //Cosmetic items
        "skinned": true,
        //Don't list certain item tags
        "tags": ["HYPERION"],
        //Finders to not list. Options: USER, CraftCost, TFM, AI, SNIPER, STONKS, FLIPPER
        "finders": ["USER", "CraftCost"],
        //If an item is in a new stack then this controls if it's listed
        //For example, if you have 1 spooky fragment in your inventory and then buy 4 you will now obviously have 5 in a stack.
        //If this is set to true, it will list the 5 fragments for the price of 1 fragment multiplied by 5.
        "stacks": false,
        //Pings you when an item doesn't list
        "pingOnFailedListing": false,
        //Doesn't list a drill if it has any parts (It will then automatically remove and sell the parts)
        "drillWithParts": false,
        //Will automatically get a price from the cofl API and relist auctions that are expired with the new price. False = don't list. True = list
        "expiredAuctions": false,
        // Will not list items in these slots. The very top left of your inventory is slot 9. The skyblock menu in the bottom right is slot 44.
        "slots": []

    },
    //Choose how long to flip for and rest for in hours.
    "autoRotate": {

        //Put your IGN (CAPS MATTER).
        //If you run multiple accounts, put a , after the value (second quote), press enter, and follow the same format as the first.
        //Add an r after the number that you want it to rest for and an F for how long you want it to flip for.

    }

    }
    """;

    public static readonly string NormalDefault = """
    {
    "igns": [""],
    "discordID": "",
    "webhook": "",
    "webhookFormat": "You bought [``{0}``](https:\/\/sky.coflnet.com\/auction\/{7}) for ``{2}`` (``{1}`` profit) in ``{4}ms``",
    "sendAllFlips": "",
    "visitFriend": "",
    "useCookie": true,
    "autoCookie": "1h",
    "angryCoopPrevention": false,
    "relist": true,
    "delay": 250,
    "waittime": 15,
    "percentOfTarget": ["0", "10b", 97],
    "listHours": ["0", "10b", 48],
    "delayTime": ["0", "10b", 0],
    "clickDelay": 125,
    "bedSpam": false,
    "blockUselessMessages": true,
    "roundTo": 6,
    "skip": {
        "always": false,
        "minProfit": "25m",
        "profitPercentage": "500",
        "minPrice": "500m",
        "userFinder": true,
        "skins": true
    },
    "doNotRelist": {
        "profitOver": "50m",
        "skinned": true,
        "tags": ["HYPERION"],
        "finders": ["USER", "CraftCost"],
        "stacks": false,
        "pingOnFailedListing": false,
        "drillWithParts": true,
        "expiredAuctions": false,
        "slots": []
    },
    "autoRotate": {
    }
    }
    """;


    public class DoNotRelist
    {
        [DataMember(Name = "profitOver")]
        public string profitOver;

        [DataMember(Name = "skinned")]
        public bool skinned;

        [DataMember(Name = "tags")]
        public string[] tags;

        [DataMember(Name = "finders")]
        public string[] finders;

        [DataMember(Name = "stacks")]
        public bool stacks;

        [DataMember(Name = "pingOnFailedListing")]
        public bool pingOnFailedListing;

        [DataMember(Name = "drillWithParts")]
        public bool drillWithParts;

        [DataMember(Name = "expiredAuctions")]
        public bool expiredAuctions;

        [DataMember(Name = "slots")]
        public int[] slots;
    }

    public class TpmPlusConfig : TpmConfig
    {
    }

    public class TpmConfig
    {
        [DataMember(Name = "igns")]
        public string[] igns;

        [DataMember(Name = "discordID")]
        public string discordID;

        [DataMember(Name = "webhooks")]
        [SettingsDoc("Comma separated list of webhooks")]
        public string[] webhooks;
        [DataMember(Name = "webhook")]
        [SettingsDoc("TPM (normal) webhook")]
        public string webhook;

        [DataMember(Name = "webhookFormat")]
        public string webhookFormat;

        [DataMember(Name = "sendAllFlips")]
        public string sendAllFlips;

        [DataMember(Name = "visitFriend")]
        public string visitFriend;

        [DataMember(Name = "useCookie")]
        public bool useCookie;

        [DataMember(Name = "autoCookie")]
        public string autoCookie;

        [DataMember(Name = "angryCoopPrevention")]
        public bool angryCoopPrevention;

        [DataMember(Name = "relist")]
        public bool relist;

        [DataMember(Name = "delay")]
        public int delay;

        [SettingsDoc("Time to wait (in seconds) after listing an item before doing the next action. Only applies to TPM (normal)")]
        [DataMember(Name = "waittime")]
        public float waittime;

        [DataMember(Name = "percentOfTarget")]
        public object[] percentOfTarget;

        [DataMember(Name = "listHours")]
        public object[] listHours;

        [DataMember(Name = "delayTime")]
        public object[] delayTime;

        [DataMember(Name = "clickDelay")]
        public int clickDelay;

        [DataMember(Name = "bedSpam")]
        public bool bedSpam;

        [DataMember(Name = "blockUselessMessages")]
        public bool blockUselessMessages;

        [DataMember(Name = "roundTo")]
        public int roundTo;

        [DataMember(Name = "skip")]
        public Skip skip;

        [DataMember(Name = "doNotRelist")]
        public DoNotRelist doNotRelist;

        [DataMember(Name = "autoRotate")]
        public Dictionary<string, string> autoRotate;

        [DataMember(Name = "session")]
        public string session;
    }


    public class Skip
    {
        [DataMember(Name = "always")]
        public bool always;

        [DataMember(Name = "minProfit")]
        public string minProfit;

        [DataMember(Name = "profitPercentage")]
        public string profitPercentage;

        [DataMember(Name = "minPrice")]
        public string minPrice;

        [DataMember(Name = "userFinder")]
        public bool userFinder;

        [DataMember(Name = "skins")]
        public bool skins;
    }

}

/// FBAF (Frikadellen BAF) default settings for the managed backend UI.
/// Analogous to TPM.NormalDefault but tuned for the Rust BAF client.
/// Field names and defaults match the Config struct in src/config/types.rs.
public class FBAF
{
    public static readonly string NormalDefault = """
    {
    "ingame_name": "",
    "multi_switch_time": 0,
    "web_gui_port": 8080,
    "command_delay_ms": 500,
    "bed_spam_click_delay": 100,
    "bed_multiple_clicks_delay": 0,
    "bed_pre_click_ms": 30,
    "bazaar_order_check_interval_seconds": 60,
    "bazaar_order_cancel_minutes_per_million": 1,
    "bazaar_tax_rate": 1.25,
    "auction_listing_delay_ms": 1500,
    "bed_spam": false,
    "skip": false,
    "use_cofl_chat": true,
    "auto_cookie": 0,
    "auction_duration_hours": 24,
    "max_items_in_inventory": 12,
    "proxy_enabled": false,
    "proxy_address": "",
    "proxy_credentials": "",
    "webhook_url": "",
    "bazaar_webhook_url": "",
    "discord_id": "",
    "share_legendary_flips": true,
    "humanization_enabled": false,
    "humanization_min_interval_minutes": 45,
    "humanization_max_interval_minutes": 120,
    "humanization_min_break_minutes": 2,
    "humanization_max_break_minutes": 10
    }
    """;
}

public class VPsStateUpdate
{
    public object Config { get; set; }
    public Instance Instance { get; set; }
    public string ExtraConfig { get; set; }
}

public class Instance
{
    public string HostMachineIp { get; set; }
    public string OwnerId { get; set; }
    public Guid Id { get; set; }
    public string AppKind { get; set; }
    public DateTime CreatedAt { get; set; }
    public DateTime PaidUntil { get; set; }
    public Dictionary<string, string> Context { get; set; }
    public string PublicIp { get; set; }
}

public class ProxyInfo
{
    public string IP { get; set; }
    public short Port { get; set; } = 1080;
    public string Username { get; set; }
    public string Password { get; set; }
    public string ProxyType { get; set; } = "socks5"; // Default to socks5
    public string Region { get; set; } = "chicago";
}
